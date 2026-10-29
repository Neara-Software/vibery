# vibery-web

Prototype of a fast Fibery web UI over the local dump and the vibery index.

    cd web/ui && pnpm install && pnpm build          # -> web/ui/dist
    cd web/server && cargo build --release && ./target/release/vibery-web   # http://127.0.0.1:7878

- `API.md`: server/UI contract. `notes/look.md`: measured Fibery look & feel, with screenshots.
- Dev: `pnpm dev` in web/ui (proxies /api to :7878).
- Doc edits are saved locally only; set `VIBERY_WRITE_THROUGH=1` to push to Fibery.
- Delete `web/cache/offsets.bin` after refreshing the dump.
