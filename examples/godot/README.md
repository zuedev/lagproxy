# lagproxy Godot demo

A two-square multiplayer scene using Godot 4's high level `ENetMultiplayerPeer`
(UDP). The host listens on `7778`; clients connect to `7777`, which is where
lagproxy sits.

```
client (Godot) --7777--> lagproxy --7778--> host (Godot)
```

## Run it

Three terminals, from the repository root:

```sh
# 1. the bad network
cargo run -- --listen :7777 --target :7778 --udp --preset wifi-bad

# 2. the host
godot --path examples/godot -- --host

# 3. a client going through the proxy
godot --path examples/godot -- --join
```

(Or run the project twice from the editor and press **Host** in one window and
**Join through proxy** in the other.)

Move with the arrow keys in the client window. The filled square is where the
server says you are; the outline is where your local prediction thinks you are.
The distance between them is the one-way latency, and every lost input packet
shows up as the filled square stalling and then catching up.

## Make it worse while it runs

```sh
curl -X POST localhost:7770/set -d '{"latency":"400ms","jitter":"150ms","loss":"10%"}'
curl -X POST localhost:7770/set -d '{"preset":"lan"}'
curl localhost:7770/stats
```

Or replay a spike with `--scenario ../../scenarios/spike.yaml --loop`, or use
`--tui` for sliders.

Note that `wifi-bad` and friends apply to the _client's_ traffic only; the host
talks to the real port and feels nothing. Run a second client through a second
proxy on another port to compare two connections side by side.
