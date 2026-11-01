# Test log stores (`query_logs`, #579–#582)

Single binaries, no Docker needed. Each store listens on localhost only and
keeps its data in a throw-away directory.

## VictoriaLogs (#580)

```sh
VL=v1.36.1
curl -fsSLO https://github.com/VictoriaMetrics/VictoriaLogs/releases/download/$VL/victoria-logs-linux-amd64-$VL.tar.gz
tar xzf victoria-logs-linux-amd64-$VL.tar.gz
./victoria-logs-prod -storageDataPath=/tmp/filar-vlogs -httpListenAddr=127.0.0.1:9428 &
curl -s --noproxy '*' http://127.0.0.1:9428/health   # OK
```

Live tests (they write their own lines through `/insert/jsonline` and find
them with a query):

```sh
FILAR_TEST_VLOGS_URL=http://127.0.0.1:9428 \
  cargo test -p filar-agent --test victorialogs -- --ignored
```

Sample data for a manual run of the agent (`shop` errors in the last hour):

```sh
now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
for i in 1 2 3 4 5; do
  printf '{"_time":"%s","_msg":"connection refused to db-%s:5432","kubernetes.pod_namespace":"shop","kubernetes.pod_name":"web-%s","level":"error"}\n' "$now" "$i" "$i"
done | curl -s --noproxy '*' -H 'Content-Type: application/stream+json' --data-binary @- \
  'http://127.0.0.1:9428/insert/jsonline?_stream_fields=kubernetes.pod_namespace,kubernetes.pod_name'
```

`Content-Type: application/stream+json` matters: without it the lines are
accepted with `200` but not ingested. A local proxy in `HTTP_PROXY` must be
bypassed (`--noproxy`), or the requests never reach the store.

```toml
[[log_sources]]
name = "vlogs-local"
kind = "victorialogs"
url = "http://127.0.0.1:9428"
```

## Grafana Loki (#582)

```sh
LOKI=v3.5.7
curl -fsSLO https://github.com/grafana/loki/releases/download/$LOKI/loki-linux-amd64.zip
unzip loki-linux-amd64.zip
cat > loki.yaml <<'YAML'
auth_enabled: false
server: {http_listen_address: 127.0.0.1, http_listen_port: 3100, grpc_listen_address: 127.0.0.1, grpc_listen_port: 9096}
common:
  instance_addr: 127.0.0.1
  path_prefix: /tmp/filar-loki
  storage: {filesystem: {chunks_directory: /tmp/filar-loki/chunks, rules_directory: /tmp/filar-loki/rules}}
  replication_factor: 1
  ring: {kvstore: {store: inmemory}}
schema_config:
  configs: [{from: 2024-01-01, store: tsdb, object_store: filesystem, schema: v13, index: {prefix: index_, period: 24h}}]
analytics: {reporting_enabled: false}
YAML
./loki-linux-amd64 -config.file=loki.yaml &
curl -s --noproxy '*' http://127.0.0.1:3100/ready   # "ready" after ~15 s
```

Live tests push their own lines through `/loki/api/v1/push`:

```sh
FILAR_TEST_LOKI_URL=http://127.0.0.1:3100 \
  cargo test -p filar-agent --test loki -- --ignored
```

Sample data:

```sh
ns=$(date +%s%N)
curl -s --noproxy '*' -H 'Content-Type: application/json' http://127.0.0.1:3100/loki/api/v1/push \
  -d '{"streams":[{"stream":{"namespace":"shop","pod":"web-1"},"values":[["'"$ns"'","level=error msg=\"connection refused to db-1:5432\""]]}]}'
```

```toml
[[log_sources]]
name = "loki-local"
kind = "loki"
url = "http://127.0.0.1:3100"
# tenant = "team-a"   # X-Scope-OrgID when auth_enabled: true
```
