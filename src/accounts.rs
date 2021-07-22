use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use actix::io::SinkWrite;
use actix::prelude::AsyncContext;
use actix::prelude::{Actor, Addr, Context, Handler, Message, Running, StreamHandler};
use actix_http::ws;
use bytes::BytesMut;
use futures_util::future::AbortHandle;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use smallvec::SmallVec;
use tokio::stream::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{DelayQueue, Instant};
use tracing::{error, info, warn};

use crate::metrics::db_metrics;
use crate::metrics::pubsub_metrics as metrics;
use crate::types::{
    AccountContext, AccountInfo, AccountsDb, Commitment, Encoding, Filter, ProgramAccountsDb,
    Pubkey, SolanaContext,
};

const MAILBOX_CAPACITY: usize = 512;
const PURGE_TIMEOUT: Duration = Duration::from_secs(600);
const IN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(60);
const WEBSOCKET_PING_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum InflightRequest {
    Sub(Subscription, Commitment),
    Unsub(Subscription, Commitment),
    SlotSub(u64),
}

type WsSink = SinkWrite<
    awc::ws::Message,
    futures_util::stream::SplitSink<
        actix_codec::Framed<awc::BoxedSocket, awc::ws::Codec>,
        awc::ws::Message,
    >,
>;

#[derive(Clone)]
pub(crate) struct PubSubManager(Vec<(actix::Addr<AccountUpdateManager>, Arc<AtomicBool>)>);

impl PubSubManager {
    pub(crate) fn init(
        connections: u32,
        accounts: AccountsDb,
        program_accounts: ProgramAccountsDb,
        websocket_url: &str,
    ) -> Self {
        let mut addrs = Vec::new();
        for id in 0..connections {
            let active = Arc::new(AtomicBool::new(false));
            let addr = AccountUpdateManager::init(
                id,
                accounts.clone(),
                program_accounts.clone(),
                Arc::clone(&active),
                &websocket_url,
            );
            addrs.push((addr, active))
        }
        PubSubManager(addrs)
    }

    fn get_idx_by_key(&self, key: Pubkey) -> usize {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        (hash % self.0.len() as u64) as usize
    }

    fn get_addr_by_key(&self, key: Pubkey) -> actix::Addr<AccountUpdateManager> {
        let idx = self.get_idx_by_key(key);
        self.0[idx].0.clone()
    }

    pub fn subscription_active(&self, key: Pubkey) -> bool {
        let idx = self.get_idx_by_key(key);
        self.0[idx].1.load(Ordering::Relaxed)
    }

    pub fn reset(&self, sub: Subscription, commitment: Commitment) {
        let addr = self.get_addr_by_key(sub.key());
        addr.do_send(AccountCommand::Reset(sub, commitment))
    }

    pub fn subscribe(
        &self,
        sub: Subscription,
        commitment: Commitment,
        filters: Option<smallvec::SmallVec<[Filter; 2]>>,
    ) {
        let addr = self.get_addr_by_key(sub.key());
        addr.do_send(AccountCommand::Subscribe(sub, commitment, filters))
    }
}

enum Connection {
    Disconnected,
    Connecting,
    Connected { sink: WsSink, stream: AbortHandle },
}

impl Connection {
    fn is_connected(&self) -> bool {
        matches!(self, Connection::Connected { .. })
    }

    fn is_connecting(&self) -> bool {
        matches!(self, Connection::Connecting { .. })
    }

    fn disconnect(&mut self) {
        let old_connection = std::mem::replace(self, Connection::Disconnected);

        if let Connection::Connected { mut sink, stream } = old_connection {
            sink.close();
            stream.abort();
        }
    }

    fn send(&mut self, msg: ws::Message) -> Result<(), ws::Message> {
        if let Connection::Connected { sink, .. } = self {
            sink.write(msg).map_or(Ok(()), Err)?;
        }
        Ok(())
    }
}

pub(crate) struct AccountUpdateManager {
    websocket_url: String,
    actor_id: u32,
    actor_name: String,
    request_id: u64,
    inflight: HashMap<u64, (InflightRequest, Instant)>,
    subs: HashSet<(Subscription, Commitment)>,
    id_to_sub: HashMap<u64, (Subscription, Commitment)>,
    sub_to_id: HashMap<(Subscription, Commitment), u64>,
    connection: Connection,
    accounts: AccountsDb,
    program_accounts: ProgramAccountsDb,
    purge_queue: DelayQueueHandle<(Subscription, Commitment)>,
    additional_keys: HashMap<Pubkey, HashSet<SmallVec<[Filter; 2]>>>,
    last_received_at: Instant,
    active: Arc<AtomicBool>,
    buffer: BytesMut,
}

impl std::fmt::Debug for AccountUpdateManager {
    fn fmt(&self, w: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        w.write_str("AccountUpdateManager{}")
    }
}

impl AccountUpdateManager {
    pub fn init(
        actor_id: u32,
        accounts: AccountsDb,
        program_accounts: ProgramAccountsDb,
        active: Arc<AtomicBool>,
        websocket_url: &str,
    ) -> Addr<Self> {
        AccountUpdateManager::create(|ctx| {
            let actor_name = format!("pubsub-{}", actor_id);
            let (handle, stream) = delay_queue(actor_name.clone());
            let purge_stream = stream.map(|(sub, com)| AccountCommand::Purge(sub, com));

            ctx.set_mailbox_capacity(MAILBOX_CAPACITY);

            ctx.run_interval(Duration::from_secs(5), move |actor, ctx| {
                if actor.connection.is_connected() {
                    if actor
                        .connection
                        .send(awc::ws::Message::Ping(b"hello?".as_ref().into()))
                        .is_err()
                    {
                        actor.disconnect(ctx);
                    }

                    let elapsed = actor.last_received_at.elapsed();
                    if elapsed > WEBSOCKET_PING_TIMEOUT {
                        warn!(
                            "no messages received in {:?}, assume connection lost ({:?})",
                            elapsed, actor.last_received_at
                        );
                        actor.disconnect(ctx);
                    }
                }
            });

            ctx.run_interval(Duration::from_secs(5), move |actor, _ctx| {
                if actor.connection.is_connected() {
                    actor.inflight.retain(|request_id, (req, time)| {
                        let elapsed = time.elapsed();
                        let too_long = elapsed > IN_FLIGHT_TIMEOUT;
                        if too_long {
                            warn!(request_id, request = ?req, timeout = ?IN_FLIGHT_TIMEOUT,
                                elapsed = ?elapsed, "request in flight too long, assume dead");
                        }
                        !too_long
                    });
                    metrics()
                        .inflight_entries
                        .with_label_values(&[&actor.actor_name])
                        .set(actor.inflight.len() as i64);
                }
            });

            AccountUpdateManager::add_stream(purge_stream, ctx);
            AccountUpdateManager {
                actor_id,
                actor_name,
                websocket_url: websocket_url.to_owned(),
                connection: Connection::Disconnected,
                id_to_sub: HashMap::default(),
                sub_to_id: HashMap::default(),
                inflight: HashMap::default(),
                subs: HashSet::default(),
                request_id: 1,
                accounts: accounts.clone(),
                program_accounts: program_accounts.clone(),
                purge_queue: handle,
                additional_keys: HashMap::default(),
                active,
                last_received_at: Instant::now(),
                buffer: BytesMut::new(),
            }
        })
    }

    fn next_request_id(&mut self) -> u64 {
        let request_id = self.request_id;
        self.request_id += 1;
        request_id
    }

    fn send<T: Serialize>(&mut self, request: &T) -> Result<(), serde_json::Error> {
        if self.connection.is_connected() {
            let _ = self
                .connection
                .send(awc::ws::Message::Text(serde_json::to_string(request)?));
        } else {
            warn!("not connected");
        }
        Ok(())
    }

    fn connect(&mut self, ctx: &mut Context<Self>) {
        use actix::fut::{ActorFuture, WrapFuture};
        use backoff::backoff::Backoff;

        let websocket_url = self.websocket_url.clone();
        let actor_id = self.actor_id;

        if self.connection.is_connected() {
            warn!(message = "old connection not canceled properly", actor_id = %actor_id);
            self.connection.disconnect();
            self.connection = Connection::Connecting;
        }

        self.update_status();

        let fut = async move {
            let mut backoff = backoff::ExponentialBackoff {
                current_interval: Duration::from_millis(300),
                initial_interval: Duration::from_millis(300),
                ..Default::default()
            };

            loop {
                info!(message = "connecting to websocket", url = %websocket_url, actor_id = %actor_id);
                let res = awc::Client::builder()
                    .max_http_version(awc::http::Version::HTTP_11)
                    .finish()
                    .ws(&websocket_url)
                    .connect()
                    .await;
                match res {
                    Ok((_, conn)) => break conn,
                    Err(err) => {
                        let delay = backoff
                            .next_backoff()
                            .unwrap_or_else(|| Duration::from_secs(1));
                        error!(message = "failed to connect, waiting", url = %websocket_url,
                                error = ?err, actor_id = %actor_id, delay = ?delay);
                        tokio::time::delay_for(delay).await;
                    }
                }
            }
        };
        let fut = fut.into_actor(self).map(|conn, actor, ctx| {
            let (sink, stream) = futures_util::stream::StreamExt::split(conn);
            let (stream, abort_handle) = futures_util::stream::abortable(stream);
            let actor_id = actor.actor_id;
            let sink = SinkWrite::new(sink, ctx);
            AccountUpdateManager::add_stream(stream, ctx);

            let old = std::mem::replace(
                &mut actor.connection,
                Connection::Connected {
                    sink,
                    stream: abort_handle,
                },
            );
            if old.is_connected() {
                warn!(actor_id, "was connected, should not have happened");
            }
            metrics()
                .websocket_connected
                .with_label_values(&[&actor.actor_name])
                .set(1);
            actor.last_received_at = Instant::now();
        });
        ctx.wait(fut);
        self.update_status();
    }

    fn subscribe(
        &mut self,
        sub: Subscription,
        commitment: Commitment,
    ) -> Result<(), serde_json::Error> {
        #[derive(Serialize)]
        struct Request<'a> {
            jsonrpc: &'a str,
            id: u64,
            method: &'a str,
            params: SubscribeParams,
        }

        #[derive(Serialize)]
        struct Config {
            commitment: Commitment,
            encoding: Encoding,
        }

        struct SubscribeParams {
            key: Pubkey,
            config: Config,
        }

        impl Serialize for SubscribeParams {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(2))?;
                seq.serialize_element(&self.key)?;
                seq.serialize_element(&self.config)?;
                seq.end()
            }
        }

        if self.subs.contains(&(sub, commitment)) {
            info!(message = "already trying to subscribe", pubkey = %sub.key());
            return Ok(());
        }

        let request_id = self.next_request_id();

        let (key, method) = match sub {
            Subscription::Account(key) => (key, "accountSubscribe"),
            Subscription::Program(key) => (key, "programSubscribe"),
        };
        info!(
            message = "subscribe to",
            pubkey = %key,
            method = method,
            commitment = ?commitment,
            request_id = request_id,
        );

        let request = Request {
            jsonrpc: "2.0",
            id: request_id,
            method,
            params: SubscribeParams {
                key,
                config: Config {
                    commitment,
                    encoding: Encoding::Base64Zstd,
                },
            },
        };

        self.inflight.insert(
            request_id,
            (InflightRequest::Sub(sub, commitment), Instant::now()),
        );
        self.subs.insert((sub, commitment));
        self.send(&request)?;
        self.purge_queue.insert((sub, commitment), PURGE_TIMEOUT);
        metrics()
            .subscribe_requests
            .with_label_values(&[&self.actor_name])
            .inc();

        Ok(())
    }

    fn unsubscribe(
        &mut self,
        sub: Subscription,
        commitment: Commitment,
    ) -> Result<(), serde_json::Error> {
        let request_id = self.next_request_id();

        #[derive(Serialize)]
        struct Request<'a> {
            jsonrpc: &'a str,
            id: u64,
            method: &'a str,
            params: [u64; 1],
        }

        if let Some(sub_id) = self.sub_to_id.get(&(sub, commitment)) {
            info!(message = "unsubscribe", key = %sub.key(), commitment = ?commitment, request_id = request_id);

            let method = match sub {
                Subscription::Program(_) => "programUnsubscribe",
                Subscription::Account(_) => "accountUnsubscribe",
            };
            let request = Request {
                jsonrpc: "2.0",
                id: request_id,
                method,
                params: [*sub_id],
            };
            self.inflight.insert(
                request_id,
                (InflightRequest::Unsub(sub, commitment), Instant::now()),
            );
            self.send(&request)?;
        }
        self.purge_key(&sub, commitment);

        Ok(())
    }

    fn purge_key(&mut self, sub: &Subscription, commitment: Commitment) {
        info!(self.actor_id, message = "purge", key = %sub.key(), commitment = ?commitment);
        match sub {
            Subscription::Program(key) => {
                let keys = self
                    .additional_keys
                    .remove(&key)
                    .into_iter()
                    .flatten()
                    .map(|filter| (key, Some(filter)))
                    .chain(Some((key, None)));
                for (key, filter) in keys {
                    if let Some(program_accounts) = self.program_accounts.remove_all(&key, filter) {
                        for key in program_accounts.into_accounts() {
                            self.accounts.remove(&key, commitment)
                        }
                    }
                }
                metrics()
                    .additional_keys_entries
                    .with_label_values(&[&self.actor_name])
                    .set(self.additional_keys.len() as i64);
            }
            Subscription::Account(key) => {
                self.accounts.remove(&key, commitment);
            }
        }
        db_metrics().account_entries.set(self.accounts.len() as i64);
        db_metrics()
            .program_account_entries
            .set(self.program_accounts.len() as i64);
    }

    fn update_status(&self) {
        let is_active = self.id_to_sub.len() == self.subs.len() && self.connection.is_connected();
        self.active.store(is_active, Ordering::Relaxed);

        metrics()
            .websocket_active
            .with_label_values(&[&self.actor_name])
            .set(if is_active { 1 } else { 0 });
    }

    fn process_ws_message(&mut self, text: &[u8]) -> Result<(), serde_json::Error> {
        #[derive(Deserialize, Debug)]
        struct AnyMessage<'a> {
            #[serde(borrow)]
            result: Option<&'a RawValue>,
            #[serde(borrow)]
            method: Option<&'a str>,
            id: Option<u64>,
            #[serde(borrow)]
            params: Option<&'a RawValue>,
            #[serde(borrow)]
            error: Option<&'a RawValue>,
        }
        let value: AnyMessage<'_> = serde_json::from_slice(&text)?;
        match value {
            // subscription error
            AnyMessage {
                error: Some(error),
                id: Some(id),
                ..
            } => {
                if let Some((req, _)) = self.inflight.remove(&id) {
                    match req {
                        InflightRequest::Sub(sub, commitment) => {
                            warn!(self.actor_id, request_id = id, error = ?error, key = %sub.key(), commitment = ?commitment, "subscribe failed");
                            metrics()
                                .subscribe_errors
                                .with_label_values(&[&self.actor_name])
                                .inc();
                            self.subs.remove(&(sub, commitment));
                        }
                        InflightRequest::Unsub(sub, commitment) => {
                            warn!(self.actor_id, request_id = id, error = ?error, key = %sub.key(), commitment = ?commitment, "unsubscribe failed");
                        }
                        InflightRequest::SlotSub(_) => {
                            warn!(self.actor_id, request_id = id, error = ?error, "slot subscribe failed");
                        }
                    }
                }
                metrics()
                    .inflight_entries
                    .with_label_values(&[&self.actor_name])
                    .set(self.inflight.len() as i64);
                self.update_status();
            }
            // subscription response
            AnyMessage {
                result: Some(result),
                id: Some(id),
                ..
            } => {
                if let Some((req, _)) = self.inflight.remove(&id) {
                    match req {
                        InflightRequest::Sub(sub, commitment) => {
                            let sub_id: u64 = serde_json::from_str(result.get())?;
                            self.id_to_sub.insert(sub_id, (sub, commitment));
                            self.sub_to_id.insert((sub, commitment), sub_id);

                            metrics()
                                .id_sub_entries
                                .with_label_values(&[&self.actor_name])
                                .set(self.id_to_sub.len() as i64);

                            metrics()
                                .sub_id_entries
                                .with_label_values(&[&self.actor_name])
                                .set(self.sub_to_id.len() as i64);

                            info!(self.actor_id, message = "subscribed to stream", sub_id = sub_id, sub = %sub, commitment = ?commitment);
                            metrics()
                                .subscriptions_active
                                .with_label_values(&[&self.actor_name])
                                .inc();
                        }
                        InflightRequest::Unsub(sub, commitment) => {
                            let is_ok: bool = serde_json::from_str(result.get())?;
                            if is_ok {
                                if let Some(sub_id) = self.sub_to_id.remove(&(sub, commitment)) {
                                    self.id_to_sub.remove(&sub_id);
                                    self.subs.remove(&(sub, commitment));
                                    info!(
                                        message = "unsubscribed from stream",
                                        sub_id = sub_id,
                                        key = %sub.key(),
                                        sub = %sub,
                                    );
                                    metrics()
                                        .subscriptions_active
                                        .with_label_values(&[&self.actor_name])
                                        .dec();
                                } else {
                                    warn!(sub = %sub, commitment = ?commitment, "unsubscribe for unknown subscription");
                                }
                                metrics()
                                    .id_sub_entries
                                    .with_label_values(&[&self.actor_name])
                                    .set(self.id_to_sub.len() as i64);

                                metrics()
                                    .sub_id_entries
                                    .with_label_values(&[&self.actor_name])
                                    .set(self.sub_to_id.len() as i64);
                            } else {
                                warn!(self.actor_id, message = "unsubscribe failed", key = %sub.key());
                            }
                        }
                        InflightRequest::SlotSub(_) => {
                            info!(self.actor_id, message = "subscribed to root");
                        }
                    }
                }
                self.update_status();
            }
            // notification
            AnyMessage {
                method: Some(method),
                params: Some(params),
                ..
            } => {
                match method {
                    "accountNotification" => {
                        #[derive(Deserialize, Debug)]
                        struct Params {
                            result: AccountContext,
                            subscription: u64,
                        }
                        let params: Params = serde_json::from_str(params.get())?;
                        if let Some((sub, commitment)) = self.id_to_sub.get(&params.subscription) {
                            //info!(key = %sub.key(), "received account notification");
                            self.accounts.insert(sub.key(), params.result, *commitment);
                            db_metrics().account_entries.set(self.accounts.len() as i64);
                        } else {
                            warn!(message = "unknown subscription", sub = params.subscription);
                        }
                        metrics()
                            .notifications_received
                            .with_label_values(&[&self.actor_name, "accountNotification"])
                            .inc();
                    }
                    "programNotification" => {
                        #[derive(Deserialize, Debug)]
                        struct Value {
                            account: AccountInfo,
                            pubkey: Pubkey,
                        }
                        #[derive(Deserialize, Debug)]
                        struct Result {
                            context: SolanaContext,
                            value: Value,
                        }
                        #[derive(Deserialize, Debug)]
                        struct Params {
                            result: Result,
                            subscription: u64,
                        }
                        let params: Params = serde_json::from_str(params.get())?;
                        if let Some((program_sub, commitment)) =
                            self.id_to_sub.get(&params.subscription)
                        {
                            let program_key = program_sub.key();
                            let key = params.result.value.pubkey;
                            let account_info = &params.result.value.account;
                            let data = &account_info.data;
                            let mut added = false;
                            if let Some(filters) = self.additional_keys.get(&program_key) {
                                for filter_group in filters {
                                    if filter_group.iter().all(|f| f.matches(data)) {
                                        added |= self.program_accounts.add(
                                            &program_key,
                                            params.result.value.pubkey,
                                            Some(filter_group.clone()),
                                            *commitment,
                                        );
                                    } else {
                                        self.program_accounts.remove(
                                            &program_key,
                                            &params.result.value.pubkey,
                                            filter_group.clone(),
                                            *commitment,
                                        );
                                    }
                                }
                            }
                            // add() returns false if we don't have an entry
                            // which means we haven't received complete result
                            // from validator by rpc
                            added |= self.program_accounts.add(
                                &program_key,
                                params.result.value.pubkey,
                                None,
                                *commitment,
                            );
                            if added {
                                self.accounts.insert(
                                    key,
                                    AccountContext {
                                        value: Some(params.result.value.account),
                                        context: params.result.context,
                                    },
                                    *commitment,
                                );
                            }
                        } else {
                            warn!(message = "unknown subscription", sub = params.subscription);
                        }
                        db_metrics().account_entries.set(self.accounts.len() as i64);
                        db_metrics()
                            .program_account_entries
                            .set(self.program_accounts.len() as i64);
                        metrics()
                            .notifications_received
                            .with_label_values(&[&self.actor_name, "programNotification"])
                            .inc();
                    }
                    "rootNotification" => {
                        #[derive(Deserialize)]
                        struct Params {
                            result: u64, //SlotInfo,
                        }
                        let params: Params = serde_json::from_str(params.get())?;
                        //info!("slot {} root {} parent {}", params.result.slot, params.result.root, params.result.parent);
                        let _slot = params.result; // TODO: figure out which slot validator *actually* reports
                    }
                    _ => {
                        warn!(message = "unknown notification", method = method);
                    }
                }
            }
            any => {
                warn!(msg = ?any, text = ?text, "unidentified websocket message");
            }
        }

        Ok(())
    }

    fn disconnect(&mut self, _ctx: &mut Context<Self>) {
        info!(self.actor_id, "websocket disconnected");
        self.connection.disconnect();

        metrics()
            .websocket_connected
            .with_label_values(&[&self.actor_name])
            .set(0);
        metrics()
            .subscriptions_active
            .with_label_values(&[&self.actor_name])
            .set(0);
        self.update_status();

        self.buffer.clear();
        self.inflight.clear();
        self.id_to_sub.clear();
        self.sub_to_id.clear();

        info!(self.actor_id, "purging related caches");
        let to_purge: Vec<_> = self.subs.iter().cloned().collect();
        for (sub, commitment) in to_purge {
            self.purge_key(&sub, commitment);
        }
    }

    fn reconnect(&mut self, ctx: &mut Context<Self>) {
        let actor_id = self.actor_id;

        if !self.connection.is_connecting() {
            info!(actor_id, "websocket reconnecting");
            if self.connection.is_connected() {
                error!(actor_id, "was connected, while doing reconnect");
                self.disconnect(ctx);
            }

            self.connect(ctx);
            self.update_status();
        } else {
            warn!(actor_id, "already reconnecting");
        }
    }
}

impl StreamHandler<AccountCommand> for AccountUpdateManager {
    fn handle(&mut self, item: AccountCommand, ctx: &mut Context<Self>) {
        let _ = <Self as Handler<AccountCommand>>::handle(self, item, ctx);
    }

    fn finished(&mut self, _ctx: &mut Context<Self>) {
        info!("purge stream finished");
    }
}

impl Handler<AccountCommand> for AccountUpdateManager {
    type Result = ();

    fn handle(&mut self, item: AccountCommand, _ctx: &mut Context<Self>) {
        let _ = (|| -> Result<(), serde_json::Error> {
            match item {
                AccountCommand::Subscribe(sub, commitment, filters) => {
                    let key = sub.key();
                    metrics()
                        .commands
                        .with_label_values(&[&self.actor_name, "subscribe"])
                        .inc();
                    self.subscribe(sub, commitment)?;
                    if let Some(filters) = filters {
                        self.additional_keys.entry(key).or_default().insert(filters);
                        metrics()
                            .additional_keys_entries
                            .with_label_values(&[&self.actor_name])
                            .set(self.additional_keys.len() as i64);
                    }
                }
                AccountCommand::Purge(sub, commitment) => {
                    metrics()
                        .commands
                        .with_label_values(&[&self.actor_name, "purge"])
                        .inc();
                    self.unsubscribe(sub, commitment)?;
                }
                AccountCommand::Reset(key, commitment) => {
                    metrics()
                        .commands
                        .with_label_values(&[&self.actor_name, "reset"])
                        .inc();
                    self.purge_queue.reset((key, commitment), PURGE_TIMEOUT);
                }
            }
            Ok(())
        })()
        .map_err(|err| {
            error!(error = %err, "error handling AccountCommand");
        });
    }
}

impl StreamHandler<Result<awc::ws::Frame, awc::error::WsProtocolError>> for AccountUpdateManager {
    fn handle(
        &mut self,
        item: Result<awc::ws::Frame, awc::error::WsProtocolError>,
        ctx: &mut Context<Self>,
    ) {
        self.last_received_at = Instant::now();

        let item = match item {
            Ok(item) => item,
            Err(err) => {
                error!(error = %err, "websocket protocol error");
                self.disconnect(ctx);
                return;
            }
        };

        let _ = (|| -> Result<(), serde_json::Error> {
            use awc::ws::Frame;

            match item {
                Frame::Ping(data) => {
                    metrics().bytes_received.with_label_values(&[&self.actor_name]).inc_by(data.len() as u64);
                    if let Connection::Connected { sink, .. } = &mut self.connection {
                        sink.write(awc::ws::Message::Pong(data));
                    }
                }
                Frame::Pong(_) => {
                    // do nothing
                }
                Frame::Text(text) => {
                    metrics().bytes_received.with_label_values(&[&self.actor_name]).inc_by(text.len() as u64);
                    self.process_ws_message(&text).map_err(|err| {
                            error!(error = %err, bytes = ?text, "error while parsing message");
                            err
                        })?
                }
                Frame::Close(reason) => {
                    warn!(reason = ?reason, "websocket closing");
                    self.disconnect(ctx);
                }
                Frame::Binary(msg) => {
                    warn!(msg = ?msg, "unexpected binary message");
                }
                Frame::Continuation(msg) => match msg {
                    ws::Item::FirstText(bytes) => {
                        metrics().bytes_received.with_label_values(&[&self.actor_name])
                            .inc_by(bytes.len() as u64);
                        self.buffer.extend(&bytes);
                    }
                    ws::Item::Continue(bytes) => {
                        metrics().bytes_received
                            .with_label_values(&[&self.actor_name])
                            .inc_by(bytes.len() as u64);
                        self.buffer.extend(&bytes);
                    }
                    ws::Item::Last(bytes) => {
                        metrics().bytes_received
                            .with_label_values(&[&self.actor_name])
                            .inc_by(bytes.len() as u64);
                        self.buffer.extend(&bytes);
                        let text = std::mem::replace(&mut self.buffer, BytesMut::new());
                        self.process_ws_message(&text).map_err(|err| {
                            error!(error = %err, bytes = ?text, "error while parsing fragmented message");
                            err
                        })?;
                    }
                    ws::Item::FirstBinary(_) => {
                        warn!(msg = ?msg, "unexpected continuation message");
                    }
                },
            }
            Ok(())
        })()
        .map_err(|err| {
            error!(message = "error handling Frame", error = ?err);
        });
    }

    fn started(&mut self, _ctx: &mut Context<Self>) {
        info!(self.actor_id, "websocket connected");
        // subscribe to slots
        let request_id = self.next_request_id();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "rootSubscribe",
        });
        self.inflight.insert(
            request_id,
            (InflightRequest::SlotSub(request_id), Instant::now()),
        );
        metrics()
            .inflight_entries
            .with_label_values(&[&self.actor_name])
            .set(self.inflight.len() as i64);

        let _ = self.send(&request);

        // restore subscriptions
        info!(self.actor_id, "adding subscriptions");
        let subs_len = self.subs.len();
        let subs = std::mem::replace(&mut self.subs, HashSet::with_capacity(subs_len));

        for (sub, commitment) in subs {
            self.subscribe(sub, commitment).unwrap()
            // TODO: it would be nice to retrieve current state for
            // everything we had before
        }
    }

    fn finished(&mut self, ctx: &mut Context<Self>) {
        info!(self.actor_id, "websocket stream finished");
        self.reconnect(ctx);
    }
}

impl Actor for AccountUpdateManager {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Context<Self>) {
        self.connect(ctx);
    }

    fn stopping(&mut self, _ctx: &mut Context<Self>) -> Running {
        info!("pubsub actor stopping");
        Running::Stop
    }

    fn stopped(&mut self, _ctx: &mut Context<Self>) {
        info!("pubsub actor stopped");
    }
}

impl actix::io::WriteHandler<awc::error::WsProtocolError> for AccountUpdateManager {
    fn error(&mut self, err: awc::error::WsProtocolError, ctx: &mut Self::Context) -> Running {
        error!(message = "websocket write error", error = ?err);
        self.disconnect(ctx);
        Running::Continue
    }

    fn finished(&mut self, _ctx: &mut Self::Context) {
        info!("writer closed");
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Hash, Copy)]
pub(crate) enum Subscription {
    Account(Pubkey),
    Program(Pubkey),
}

impl Subscription {
    pub fn key(&self) -> Pubkey {
        match *self {
            Subscription::Account(key) => key,
            Subscription::Program(key) => key,
        }
    }
}

impl std::fmt::Display for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (prefix, key) = match self {
            Subscription::Account(key) => ("Account", key),
            Subscription::Program(key) => ("Program", key),
        };
        write!(f, "{}({})", prefix, key)
    }
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub(crate) enum AccountCommand {
    Subscribe(Subscription, Commitment, Option<SmallVec<[Filter; 2]>>),
    Reset(Subscription, Commitment),
    Purge(Subscription, Commitment),
}

enum DelayQueueCommand<T> {
    Insert(T, Instant),
    Reset(T, Instant),
}

struct DelayQueueHandle<T>(mpsc::UnboundedSender<DelayQueueCommand<T>>);

impl<T> DelayQueueHandle<T> {
    fn insert_at(&self, item: T, time: Instant) {
        let _ = self.0.send(DelayQueueCommand::Insert(item, time));
    }

    fn insert(&self, item: T, dur: Duration) {
        self.insert_at(item, Instant::now() + dur)
    }

    fn reset(&self, item: T, dur: Duration) {
        let _ = self
            .0
            .send(DelayQueueCommand::Reset(item, Instant::now() + dur));
    }
}

fn delay_queue<T: Clone + std::hash::Hash + Eq>(
    id: String,
) -> (DelayQueueHandle<T>, impl Stream<Item = T>) {
    let (sender, incoming) = mpsc::unbounded_channel::<DelayQueueCommand<T>>();
    let mut map: HashMap<T, _> = HashMap::default();
    let stream = stream_generator::generate_stream(|mut stream| async move {
        let mut delay_queue = DelayQueue::new();
        tokio::pin!(incoming);

        loop {
            metrics()
                .purge_queue_length
                .with_label_values(&[&id])
                .set(delay_queue.len() as i64);
            metrics()
                .purge_queue_entries
                .with_label_values(&[&id])
                .set(map.len() as i64);
            tokio::select! {
                item = incoming.next() => {
                    if let Some(item) = item {
                        match item {
                            DelayQueueCommand::Insert(item, time) => {
                                map.insert(item.clone(), delay_queue.insert_at(item, time));
                            },
                            DelayQueueCommand::Reset(item, time) => {
                                if let Some(key) = map.get(&item) {
                                    delay_queue.reset_at(&key, time);
                                } else {
                                    map.insert(item.clone(), delay_queue.insert_at(item, time));
                                }
                            }
                        }
                    } else {
                        break;
                    }
                }
                out = delay_queue.next(), if !delay_queue.is_empty() => {
                    if let Some(Ok(out)) = out {
                        let item = out.into_inner();
                        map.remove(&item);
                        stream.send(item).await;
                    }
                }
            }
        }
    });
    (DelayQueueHandle(sender), stream)
}
