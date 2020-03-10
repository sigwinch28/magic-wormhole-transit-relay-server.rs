# Magic Wormhole Transit Relay Server in Rust

This is a prototype replacement for the [Magic Wormhole Transit Relay Server](https://github.com/warner/magic-wormhole-transit-relay) written in Rust.

Building:
```
$ cargo build
```

Running:
```
$ cargo run
server running on [::]:4001
```

## Features:

* [ ] handshakes
  + [ ] old: `please relay ${token}`
  + [x] new: `please relay ${token} for side ${side}`
* [ ] usage statistics
* [ ] authentication
* [ ] impatience detection
* [ ] request logging
* [ ] idle connection pruning
