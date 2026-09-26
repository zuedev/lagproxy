# lagproxy

> Simulate bad network conditions for your game client.

A cross-platform command line proxy that sits between your game client and server and makes the network worse on purpose.

Add latency, jitter, packet loss, duplication, reordering and bandwidth limits to UDP or TCP traffic without touching your game code. Test how your netcode behaves on a bad hotel wifi connection while sitting on a good one.

```
lagproxy --listen 0.0.0.0:7777 --target 127.0.0.1:7778 --udp \
  --latency 120ms --jitter 40ms --loss 3% --reorder 1%
```

Point your client at port 7777. Traffic is forwarded to your real server on 7778 with the conditions applied in both directions.

## Why

Most multiplayer bugs only show up on bad connections. Prediction glitches, rubber banding, desyncs, timeouts and reconnect failures are all invisible when you test on localhost. Existing tools are either platform specific (`tc` on Linux, Network Link Conditioner on macOS, Clumsy on Windows), require admin rights, or affect the whole machine.

lagproxy is a single binary that runs anywhere, needs no privileges, affects only the traffic you route through it, and can change conditions live while your game is running.

## Features

- UDP and TCP forwarding
- Independent settings for upstream and downstream traffic
- Latency, jitter, packet loss, duplication, reordering, corruption
- Bandwidth throttling with a configurable queue size
- Scripted scenarios: change conditions over time from a file
- Live control over a local HTTP API or interactive TUI
- Presets for common real world conditions
- Per-packet logging and summary stats on exit
- Single static binary for Windows, macOS and Linux

## Install

Download a binary from [Releases](../../releases), or:

```
cargo install lagproxy
```

Or with Docker:

```
docker run --rm -p 7777:7777/udp ghcr.io/zuedev/lagproxy \
  --listen 0.0.0.0:7777 --target host.docker.internal:7778 --udp --preset wifi-bad
```

## Usage

### Basic

```
lagproxy --listen <addr:port> --target <addr:port> [--udp|--tcp] [conditions]
```

| Flag                            | Description                                               | Example        |
| ------------------------------- | --------------------------------------------------------- | -------------- |
| `--latency`                     | Fixed one-way delay                                       | `80ms`         |
| `--jitter`                      | Random delay added on top of latency                      | `30ms`         |
| `--jitter-dist`                 | Distribution for jitter: `uniform`, `normal`, `pareto`    | `normal`       |
| `--loss`                        | Chance each packet is dropped                             | `2%`           |
| `--loss-burst`                  | Chance the next packet is also dropped after a drop       | `50%`          |
| `--dup`                         | Chance each packet is sent twice                          | `0.5%`         |
| `--reorder`                     | Chance a packet is held and sent after the next one       | `1%`           |
| `--corrupt`                     | Chance a random byte is flipped                           | `0.1%`         |
| `--bandwidth`                   | Cap throughput                                            | `1mbit`        |
| `--queue`                       | Max bytes buffered when over bandwidth, excess is dropped | `64kb`         |
| `--up:<flag>` / `--down:<flag>` | Apply a condition in one direction only                   | `--up:loss 5%` |

Durations accept `ms` and `s`. Percentages accept `%` or a decimal (`0.02`).

### Presets

```
lagproxy --listen :7777 --target :7778 --udp --preset mobile-4g
```

| Preset         | Roughly                                                       |
| -------------- | ------------------------------------------------------------- |
| `lan`          | 1ms, no loss                                                  |
| `fibre`        | 15ms, 2ms jitter                                              |
| `cable`        | 40ms, 10ms jitter, 0.1% loss                                  |
| `wifi-ok`      | 50ms, 20ms jitter, 0.5% loss                                  |
| `wifi-bad`     | 120ms, 60ms jitter, 3% loss, 2% reorder                       |
| `mobile-4g`    | 90ms, 40ms jitter, 1% loss, 5mbit                             |
| `mobile-3g`    | 250ms, 100ms jitter, 3% loss, 500kbit                         |
| `satellite`    | 600ms, 30ms jitter, 0.5% loss                                 |
| `cross-region` | 180ms, 15ms jitter, 0.2% loss                                 |
| `hostile`      | 300ms, 200ms jitter, 10% loss, 5% dup, 5% reorder, 1% corrupt |

Presets are a starting point. Any flag given after `--preset` overrides it.

### Scenarios

Change conditions over time from a file. Useful for reproducing a specific bug or for automated tests.

```yaml
# spike.yaml
- at: 0s
  latency: 40ms
  jitter: 5ms
- at: 10s
  latency: 400ms
  jitter: 150ms
  loss: 8%
- at: 13s
  latency: 40ms
  jitter: 5ms
- at: 20s
  loss: 100% # simulate a full disconnect
- at: 25s
  loss: 0%
- at: 30s
  end: true
```

```
lagproxy --listen :7777 --target :7778 --udp --scenario spike.yaml --loop
```

### Live control

Every running instance exposes a local API on `127.0.0.1:7770` by default.

```
curl -X POST localhost:7770/set -d '{"latency":"200ms","loss":"5%"}'
curl localhost:7770/stats
```

Or run with `--tui` for an interactive view where you can adjust sliders and watch throughput, drops and queue depth in real time.

### Multiple clients

When testing more than one client against the same server, run one proxy per client so each gets its own conditions:

```
lagproxy --listen :7777 --target :7800 --udp --preset fibre
lagproxy --listen :7778 --target :7800 --udp --preset mobile-3g
```

## Engine notes

**Godot**: set your `ENetMultiplayerPeer` or `WebSocketPeer` client address to the proxy's listen port. Nothing else changes. There is a runnable two-player example in [examples/godot](examples/godot/README.md).

**Unity (Netcode for GameObjects / Unity Transport)**: point the client connection data at the proxy. Note that Unity Transport has its own basic simulator pipeline; lagproxy is useful when you want the same conditions across engines or need scenarios and live control.

**Unreal**: set the client's connect URL to the proxy address. Unreal's `Net PktLag` console commands are an alternative for quick tests but only affect one process.

**Steam Datagram Relay / Epic Online Services**: these tunnel through their own relays and cannot be proxied this way. Test the fallback direct-connect path, or use their built-in simulation options.

## Stats

On exit, or via `/stats`, you get a summary:

```
Direction   Packets   Bytes     Dropped   Dup   Reordered   Avg delay   Max queue
up          48,211    5.1 MB    1,442     240   482         121ms       12 KB
down        96,904    31.7 MB   2,901     0     969         118ms       48 KB
```

Use `--log packets.jsonl` to write every packet decision to a file for later analysis.

## How it works

Each incoming packet is stamped, run through the condition chain (loss, dup, corrupt, reorder, then delay), and placed on a min-heap keyed by release time. A single timer thread pops packets when they're due and forwards them. Bandwidth limiting is a token bucket ahead of the delay heap; when the bucket and queue are both exhausted, packets are dropped and counted.

TCP mode operates on the byte stream, so "packet" means a read chunk. Loss in TCP mode delays rather than drops, since dropping bytes from a stream would just break the connection. Use `--tcp-strict` if you actually want it to break.

## Limitations

- Does not simulate MTU fragmentation or path MTU changes
- Does not modify IP-level headers, so tools that inspect TTL or DSCP will not see changes
- Cannot proxy traffic that goes through third party relays
- Timer resolution is limited by the OS, typically 1ms on Linux and macOS and around 1ms on Windows with high resolution timers enabled

## Contributing

Issues and pull requests welcome. If you have a real-world capture of bad network conditions you'd like turned into a preset, open an issue with the numbers.

```
git clone https://github.com/zuedev/lagproxy
cd lagproxy
cargo test
cargo run -- --listen :7777 --target :7778 --udp --preset wifi-bad
```

`cargo bench` measures the per-packet cost of the condition chain and scheduler, and the overhead of putting the proxy in the path on loopback (UDP round trip and TCP throughput, direct vs proxied).

## Licence

This project is licensed under the Unlicense, dedicating it to the public domain. For more information, please refer to [LICENSE](LICENSE) for details.
