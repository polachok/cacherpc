use std::future::Future;
use std::time::Duration;

use actix::fut::{ActorFuture, WrapFuture};
use actix::prelude::{Actor, Addr, AsyncContext, Context, Handler, Message};
use awc::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::metrics::rpc_metrics as metrics;
use crate::rpc::{Id, Request};
use crate::types::Commitment;

#[derive(Serialize)]
struct Param {
    commitment: Commitment,
}
#[derive(Deserialize, Debug)]
struct Wrap<T> {
    #[serde(flatten)]
    inner: Response<T>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
enum Response<T> {
    Result(T),
    Error(Box<serde_json::value::RawValue>),
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
enum MonitorMessage {
    SlotUpdated(u64),
    HealthUpdated(bool),
}

pub struct RpcMonitor {
    client: Client,
    rpc_url: String,
    id: u64,
}

impl RpcMonitor {
    pub fn init(url: &str, client: Client) -> Addr<Self> {
        let actor = Self::new(url, client);
        actor.start()
    }

    fn new(url: &str, client: Client) -> Self {
        RpcMonitor {
            client,
            rpc_url: url.to_owned(),
            id: 1,
        }
    }

    fn request_id(&mut self) -> u64 {
        let next_id = self.id;
        self.id += 1;
        next_id
    }

    fn get_health(&mut self) -> impl Future<Output = anyhow::Result<bool>> {
        let request: Request<'_, ()> = Request {
            jsonrpc: "2.0",
            id: Id::Num(self.request_id()),
            method: "getHealth",
            params: None,
        };

        let client = self.client.clone();
        let rpc_url = self.rpc_url.clone();

        async move {
            Self::request::<_, String>(request, client, rpc_url)
                .await
                .map(|res| res == "ok")
        }
    }

    fn get_slot(&mut self) -> impl Future<Output = anyhow::Result<u64>> {
        let request = Request {
            jsonrpc: "2.0",
            id: Id::Num(self.request_id()),
            method: "getSlot",
            params: Some(&[Param {
                commitment: Commitment::Processed,
            }]),
        };

        let client = self.client.clone();
        let rpc_url = self.rpc_url.clone();

        Self::request::<_, u64>(request, client, rpc_url)
    }

    async fn request<Req: Serialize, Resp: serde::de::DeserializeOwned>(
        request: Req,
        client: Client,
        url: String,
    ) -> anyhow::Result<Resp> {
        let mut resp = client
            .post(&url)
            .send_json(&request)
            .await
            .map_err(|err| anyhow::Error::msg(err.to_string()))?;
        let resp = resp.json::<Wrap<Resp>>().await?;
        match resp.inner {
            Response::Result(resp) => Ok(resp),
            Response::Error(err) => {
                anyhow::bail!(err);
            }
        }
    }
}

impl Actor for RpcMonitor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Context<Self>) {
        ctx.run_interval(Duration::from_secs(1), move |actor, ctx| {
            let fut = actor
                .get_slot()
                .into_actor(actor)
                .map(|result, _actor, ctx| match result {
                    Ok(slot) => ctx.notify(MonitorMessage::SlotUpdated(slot)),
                    Err(err) => {
                        warn!(%err, "error updating rpc slot");
                    }
                });
            ctx.wait(fut);
        });

        ctx.run_interval(Duration::from_secs(1), move |actor, ctx| {
            let fut =
                actor
                    .get_health()
                    .into_actor(actor)
                    .map(|result, _actor, ctx| match result {
                        Ok(healthy) => ctx.notify(MonitorMessage::HealthUpdated(healthy)),
                        Err(err) => {
                            warn!(%err, "error updating rpc health");
                        }
                    });
            ctx.wait(fut);
        });
    }
}

impl Handler<MonitorMessage> for RpcMonitor {
    type Result = ();

    fn handle(&mut self, item: MonitorMessage, _ctx: &mut Context<Self>) {
        match item {
            MonitorMessage::SlotUpdated(slot) => {
                debug!(%slot, "rpc slot updated");
                metrics().rpc_slot.set(slot as i64);
            }
            MonitorMessage::HealthUpdated(health) => {
                debug!(healthy = %health, "rpc health updated");
            }
        }
    }
}
