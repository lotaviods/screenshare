# Sharescreen infrastructure

Docker Compose setup for the signaling server and TURN/STUN. The desktop app
also works against externally hosted infrastructure — nothing here is required
to run the app.

## Services

| Service | Purpose | Default ports |
| --- | --- | --- |
| `signaling` | Room management, WebRTC signaling, TURN credential issuance | `8080` |
| `coturn` | STUN discovery + TURN relay fallback | `3478/udp+tcp`, `5349/tcp`, `49152-49251/udp` |
| `sfu` | Optional SFU (profile `sfu`, off by default) | `7880`, `7881/udp` |

Room state is kept in memory, so no database service is required. The signaling
server never proxies media — peers connect directly, or through TURN when a
direct path is not possible.

## Running

```sh
cp deploy/.env.example deploy/.env
# set TURN_SECRET to a long random value (openssl rand -hex 32)
docker compose -f deploy/compose.yaml --env-file deploy/.env up -d
docker compose -f deploy/compose.yaml ps

Deploy target: **192.168.1.200**. The `signaling` service uses `restart: always`,
so Docker restarts it after a crash or host reboot.
```

TURN credentials are issued on demand by the signaling server using coturn's
shared-secret scheme: the username is `<expiry>:<user>` and the credential is
`base64(HMAC-SHA1(secret, username))`. Clients fetch them over the signaling
WebSocket (`GetTurnCredentials`) or over HTTP:

```sh
curl http://localhost:8080/turn-credentials
```

`TURN_SECRET` must be identical in the signaling server and coturn — the
signaling server signs with it and coturn verifies.

## Connecting the app

Point the app at the server in **Settings → Network**, or edit
`~/.config/sharescreen/config.toml`:

```toml
[network]
signaling_host = "127.0.0.1"
signaling_port = 8080
signaling_secure = false
```

## Development

The compose file is not required for local development — run the server
directly:

```sh
cargo run -p sharescreen-server
```

The `signaling` image contains no curl or wget; its health check uses the
server binary itself (`sharescreen-server --health-check`), which probes its
own listening socket.

## TURN over TLS

The compose file publishes `5349/tcp` for `turns:`. TLS needs certificates:
mount them into the coturn container and add `--cert=` / `--pkey=` to its
`command` arguments. Without certificates, coturn listens on 5349 but rejects
TLS handshakes, so clients fall back to plain `turn:` over UDP/TCP.
