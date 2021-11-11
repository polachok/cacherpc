use cache_rpc::{init_lua, rpc::Request};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

fn ruleset_path() -> String {
    std::env::var("RULESET").expect("missing env RULESET")
}

fn data_path() -> String {
    std::env::var("DATA").expect("missing env DATA")
}

fn bench_lua_rule(c: &mut Criterion) {
    use criterion::Throughput;
    let mut group = c.benchmark_group("Lua/Real");

    let lua_rules = std::fs::read_to_string(&ruleset_path()).unwrap();
    let lua = init_lua(&lua_rules).unwrap();
    let data = std::fs::read_to_string(&data_path()).unwrap();
    let lines = data.split('\n').collect::<Vec<_>>();

    group.throughput(Throughput::Elements(lines.len() as u64));

    group.bench_function(BenchmarkId::new("Deserialize", lines.len()), |b| {
        b.iter(|| {
            for line in &lines {
                if line.is_empty() {
                    continue;
                }
                let _req: Request<serde_json::value::RawValue> = serde_json::from_str(&line)
                    .map_err(|err| {
                        println!("failed on {}", line);
                        err
                    })
                    .unwrap();
            }
        })
    });

    group.bench_function(BenchmarkId::new("DeserializeAndLua", lines.len()), |b| {
        b.iter(|| {
            for line in &lines {
                if line.is_empty() {
                    continue;
                }
                let req: Request<serde_json::value::RawValue> =
                    serde_json::from_str(&line).unwrap();
                let _res = lua.scope(|scope| {
                    lua.globals()
                        .set("request", scope.create_nonstatic_userdata(&req)?)?;
                    lua.load("require 'waf'.request(request)").eval::<bool>()
                });
            }
        })
    });
}

criterion_group!(lua_benches, bench_lua_rule);
criterion_main!(lua_benches);
