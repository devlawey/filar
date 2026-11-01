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
