# Using Ellmephant with TypingMind

TypingMind is a web-based chat app, and it can talk to Ellmephant — but it needs a little help.
TypingMind doesn't speak MCP directly; it goes through a **connector**. Getting that connector
right is the whole game, so let's do it the safe way.

## The short version

**Don't use TypingMind's hosted connector.** That one does the connecting from TypingMind's own
cloud, which means Ellmephant has to be reachable from the open internet — and a server with no
auth or TLS has no business being out there.

Instead, you should run TypingMind's own open-source [MCP
Connector](https://github.com/TypingMind/typingmind-mcp) right next to
Ellmephant. We ship a compose for exactly that:
[`compose.typingmind.yaml`](compose.typingmind.yaml). It starts both.
Ellmephant stays on the internal Docker network and is never published; only
the connector is exposed, on `127.0.0.1`, where TypingMind can reach it.

## Start the stack

Pick a secret (any long random string — TypingMind calls it the *auth token*) and bring it up:

```sh
MCP_AUTH_TOKEN="$(openssl rand -hex 32)" podman compose -f compose.typingmind.yaml up -d
```

Echo that token somewhere — you'll paste it into TypingMind in a moment. Docker works just the
same: `docker compose -f compose.typingmind.yaml up -d`.

## Wire it into TypingMind

1. Open **Settings → Advanced Settings → Model Context Protocol** and choose the **Private MCP
   Connector** (not TypingMind Cloud).
2. Connector URL: `http://localhost:50880`. Auth token: the `MCP_AUTH_TOKEN` you picked.
3. Once it connects, click **Edit Servers** and add Ellmephant. The connector runs *inside* the
   compose network, so point it at Ellmephant's service name — **not** `localhost`:

   ```jsonc
   {
     "mcpServers": {
       "ellmephant": { "url": "http://ellmephant:8888/mcp" }
     }
   }
   ```

4. Save. Ellmephant's memory tools show up on the **Plugins** page, ready to use like any other.

## Troubleshooting

**The connector won't connect over `http://localhost`.**
TypingMind runs over HTTPS, and browsers block an HTTPS page from calling plain `http://localhost`
(Safari is the strict one). Give the connector a locally-trusted cert — `mkcert localhost` makes a
`localhost.pem` + `localhost-key.pem` pair — mount the pair into the container, and switch on
`CERTFILE` / `KEYFILE`. The commented lines in [`compose.typingmind.yaml`](compose.typingmind.yaml)
show where.

**The connector runs, but Ellmephant's tools never show up.**
Check the server config points at `http://ellmephant:8888/mcp` — the *service name*, not
`localhost`. The connector resolves that from inside the compose network, where `localhost` means
the connector's own container.

**Still nothing?**
Watch the connector's logs and make sure Ellmephant itself came up:

```sh
podman compose -f compose.typingmind.yaml logs -f mcp-connector
```
