# remote-steer

[English](README.md) | [한국어](README.ko.md)

![remote-steer hero](docs/assets/remote-steer-hero.png)

[![Release](https://img.shields.io/github/v/release/smturtle2/remote-steer?style=flat-square)](https://github.com/smturtle2/remote-steer/releases)
[![License](https://img.shields.io/github/license/smturtle2/remote-steer?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.82%2B-f74c00?style=flat-square&logo=rust)](https://www.rust-lang.org/)
[![Windows server](https://img.shields.io/badge/server-Windows-0078d4?style=flat-square)](#supported-setup)
[![Linux client](https://img.shields.io/badge/client-Linux-fcc624?style=flat-square)](#supported-setup)

`remote-steer` is a Rust CLI that bridges a physical Thrustmaster T150/T150RS
on a Windows machine to a virtual T150 on a Linux gaming machine, including
force-feedback forwarding.

It is built for the case where the wheel is attached to one computer, but the
game needs to run on another.

## Supported Setup

| Role | Current support |
| --- | --- |
| Physical wheel machine | Windows 10/11 with a Thrustmaster T150/T150RS |
| Game / virtual wheel machine | Linux with `uinput` and evdev access |
| Wheel profile | Thrustmaster T150RS (`044f:b677`) |
| Transport | UDP, default port `43150` |
| Force feedback | Constant, periodic, spring, damper, gain, autocenter, sine, saw up/down |

This is an early hardware-specific release. It does not claim general wheel
support yet.

## Quick Start

Run the server once on the Windows machine with the real wheel attached:

```sh
remote-steer server --token <shared-token>
```

Run the client once on the Linux machine where the game runs:

```sh
remote-steer client <windows-host-or-ip> --token <shared-token>
```

After the first successful connection, `remote-steer` remembers the server and
token. Daily use becomes:

```sh
# Windows wheel machine
remote-steer server start

# Linux game machine
remote-steer client start
```

The Linux side creates a virtual Thrustmaster T150 input device. Start the game
after the client is connected and select the virtual wheel.

Use `remote-steer server status`, `remote-steer server stop`,
`remote-steer client status`, and `remote-steer client stop` to manage the
background processes.

## Force-Feedback Test

To test force feedback directly through the remote Windows wheel server:

```sh
remote-steer test
```

To play one preset and exit:

```sh
remote-steer test --effect engine
```

The test menu includes the same style of named effects exposed by the
Thrustmaster control panel:

`Engine`, `Blown Tire`, `Boing`, `Explosion`, `Open Sea`, `Turbo Boost`, `Gong`,
`Bumpy Road`, `Car Crash`, `Punch`, `Force Field`, and `Whiplash`.

You can also use Linux tools against the virtual event device:

```sh
fftest /dev/input/eventXX
```

Spring and damper effects are condition effects, so they are felt while turning
the wheel rather than as self-moving kicks.

## Installation

Download the latest release from
[GitHub Releases](https://github.com/smturtle2/remote-steer/releases).

For this release, use:

- `remote-steer-v0.0.2-windows-x86_64.zip` on the Windows wheel machine
- `remote-steer-v0.0.2-linux-x86_64.tar.gz` on the Linux game machine
- `SHA256SUMS` to verify downloaded files

Build from source:

```sh
cargo build --release -p remote-steer
cargo build --release --target x86_64-pc-windows-gnu -p remote-steer
```

## CLI Reference

```sh
remote-steer server --token <shared-token>
remote-steer server
remote-steer server start
remote-steer server status
remote-steer server stop
remote-steer client <server-host-or-ip> --token <shared-token>
remote-steer client
remote-steer client start
remote-steer client status
remote-steer client stop
remote-steer test
remote-steer test --effect engine
remote-steer probe physical
remote-steer probe virtual
remote-steer dump-direct-input
```

Connection defaults are saved after a successful first run. Override them with
`REMOTE_STEER_SERVER`, `REMOTE_STEER_TOKEN`, or `REMOTE_STEER_CONFIG`.

The old `physical`, `virtual`, and `test-ffb` subcommands remain as hidden
compatibility commands. New users should use `server`, `client`, and `test`.

## Architecture

- `remote-steer-core`: T150 profile, wheel state, force-feedback commands, and backend traits.
- `remote-steer-transport`: authenticated UDP packet format and message transport.
- `remote-steer-backend-windows`: DirectInput physical-wheel backend.
- `remote-steer-backend-linux`: Linux uinput/evdev virtual-wheel backend.
- `remote-steer-cli`: user-facing command-line application.

## Security Notes

The token is a shared secret for packet authentication. It is not a full
transport security layer. Use `remote-steer` only on a trusted LAN, private VPN,
or a network path you control.

## Development Checks

```sh
cargo fmt --all --check
cargo test --workspace
cargo check --target x86_64-pc-windows-gnu -p remote-steer
```

## License

MIT. See [LICENSE](LICENSE).
