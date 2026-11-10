<p align="center">
  <img src="docs/assets/benilla-wotlk-banner.png" alt="Benilla WotLK" width="100%">
</p>

# Wenilla-Wotlk

This fork of [Benilla-Wotlk](https://github.com/Onixaz/benilla-wotlk) compiles the same client to WebAssembly, after
[wenilla](https://github.com/Arnesen/wenilla): raw TCP becomes a WebSocket through a small local
host (`wenilla-host`), the MPQ chain becomes files fetched over HTTP (batched, fetched ahead and
kept in the browser between visits), and the C Lua 5.1 VM is built for wasm against the wasi-sdk
sysroot. WebGPU is required for the world.

### What you need

- Rust (the toolchain in `rust-toolchain.toml` installs itself) and about 10 GB free for `target/`.
- Your own 3.3.5a (build 12340) install's `Data` folder, with its locale folders (`Data/enUS`,
  `Data/esES`, …). The host serves every one it finds.
- A 3.3.5a server you can reach: auth on 3724 and world on 8085 (AzerothCore, TrinityCore, …),
  with Warden off.
- A WebGPU browser: Chrome or Edge, or Safari 26+.

### Start it

```bash
# 1. Once: the wasm target, wasm-bindgen, wasi-sdk and binaryen (into tools/, no sudo).
scripts/web-setup.sh

# 2. Build the page into web/dist/ (5-10 minutes; again after every client change).
scripts/web-build.sh

# 3. Serve it: the page, your game data and the connection to your server.
cargo run -p wenilla-host -- --www web/dist --data /path/to/WoW/Data --upstream <server ip>
```

4. Open **http://127.0.0.1:8090/** and log in as you would in the game. On a first visit the
   realm wizard comes up once: pick your realm. To skip the login screen while testing, add
   `?user=<account>&pass=<password>&char=<character>` to the address.

After a rebuild (`scripts/web-build.sh`), reload the page; the host only needs restarting when
its own code changes. `--bind <addr:port>` serves on another address or port (`127.0.0.1:8090` by
default); `--upstream` is the server the host connects your game session to.

Every locale folder in `Data` is served. A player gets the one their browser's language asks for
when it is installed, or `?locale=esES` in the address, or the one they pick in the game's
Languages options (it applies on reload); otherwise the host's default, `enUS` unless you pass
`--locale <folder>`. Adding a locale folder leaves the data players already downloaded valid.

The first visit downloads the game data the world needs as you go; later visits read it from the
browser's own storage and start much faster. Clearing the site's data in the browser resets
that, and your settings and key bindings with it.

`wenilla-host` is a local testing tool: it serves the game data you point it at with no login,
so keep it on loopback. See [`web/README.md`](web/README.md) for how the port works.


## Acknowledgements and legal

Wenilla-Wotlk is an independent fan project and is not affiliated with or endorsed by Blizzard
Entertainment. It ships no assets extracted from World of Warcraft: no models, sounds, maps, MPQ
contents, or FrameXML. You must provide your own legally obtained 3.3.5a installation. The files
under `crates/benilla-app/assets/ui/` are project-owned adapters and interface elements, not copies
from the game.

World of Warcraft is a trademark of Blizzard Entertainment, Inc. Benilla-Wotlk code is available under
your choice of the [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE) license. Vendored components
under `third_party/`, including the Kira audio engine and Lua 5.1, retain their own licenses.
