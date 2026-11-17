# CF-WispJS

A Wisp server running on Cloudflare Workers.

Visiting the Worker over HTTPS returns a plaintext health page. Wisp clients connect to the
WebSocket endpoint on the same host:

```
wss://<worker-host>/
```

```js
import { client as wisp } from "@mercuryworkshop/wisp-js/client";

const conn = new wisp.ClientConnection("wss://<worker-host>/");
```

This is meant for small API clients which do not need many parallel requests or open
connections: TCP streams only, six streams per connection, and one stream per `host:port`.
Do not try to use this for any other purpose.
