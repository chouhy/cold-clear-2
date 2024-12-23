# Cold Clear 2

Cold Clear 2 is a modern Tetris versus bot and a complete rewrite and evolution
of [Cold Clear](https://github.com/MinusKelvin/cold-clear). It implements the
[Tetris Bot Protocol](https://github.com/tetris-bot-protocol/tbp-spec) for
interaction with a frontend, such as [Quadspace](https://github.com/SoRA-X7/Quadspace).

## Technical Features

- Column-major bitboards
- Multithreaded search
- Transposition-aware game tree
- MCTS-inspired tree expansion

## Compile

- Can be compiled in normal rust way.
- Can also compiled to WASM. Generate .wasm and .js by `wasm-pack build --no-typescript --target no-modules`. Check pkg/worker.js for usage.


## License

Cold Clear 2 is licensed under either [Apache License Version 2.0](LICENSE-APACHE)
or [MIT License](LICENSE-MIT), at your option.
