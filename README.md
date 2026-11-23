# NIFTY agentic paper trader (Rust / Termux)

Paper-only starter for NIFTY research. It starts with ₹10,000 of simulated capital and has no broker order-placement code. Live order APIs are deliberately absent. The `IndependentStrategy` adapter has no webhook path and defaults to a no-op until you connect your existing strategy implementation.

## What is included

- Hybrid scheduling primitives: market-data events plus five-minute context ticks.
- Groq structured JSON client and a 10 worker → 2 subleader → 1 Driver hierarchy. Workers share one objective and receive distinct analytical lenses; response rate and concurrent calls are bounded in settings.
- Read-only INDstocks REST adapter scaffold, raw WebSocket URL configuration and explicit endpoint/auth/schema TODOs. No requests are sent until enabled.
- Deterministic freshness, position, per-trade capital, daily-loss and short-selling gates.
- SQLite signal/fill journal with duplicate signal IDs ignored.
- Paper broker applies configured spread, slippage, fees and partial fills.
- Safety tests for stale data, duplicate signals, capital limits and incomplete worker cycles.

## Termux setup

Install current Termux from a maintained source, then:

```sh
pkg update
pkg install rust clang make pkg-config openssl
cd nifty-paper-agent
cp .env.example .env
chmod 600 .env
nano .env
nano settings.toml
cargo test
cargo run --release
```

## Windows command prompt check

Install Rust with rustup and the Visual Studio C++ Build Tools workload (MSVC linker and Windows SDK). Open the Visual Studio 2022 x64 Native Tools Command Prompt, or initialize the toolchain in Command Prompt with:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
git clone https://github.com/dineshrameshhere-oss/nifty-paper-agent.git
cd nifty-paper-agent
cargo test
cargo run
```

If `rustc` or `cargo` is not recognized, close and reopen Command Prompt after installing Rust so it reloads the updated PATH.

Put the INDstocks read-only token and Groq key in `.env`, not in source or `settings.toml`. Replace all `SET_GROQ_*_MODEL` values with model IDs currently available to your Groq account. Set `market_data_enabled = true` only after verifying the official INDstocks API version, URL, authentication, quote schema, instrument identifier, WebSocket subscription payload, and rate limits. The current REST scaffold assumes a bearer token and a simple quote JSON object; that assumption is marked for verification and may need adaptation. The WebSocket URL is configuration only: subscription and message decoding remain TODOs until the provider schema is confirmed.

The executable currently validates configuration, opens the SQLite journal and exits safely. This is intentional while the provider-specific schema and your existing strategy adapter are unconfigured. Wire verified data into `HybridScheduler`, call `GroqOrchestrator::analyze` per tick, and route only validated paper signals through `PaperBroker` and `Journal`. The Driver is configured to return an assessment only; it is instructed to keep `signal=null`, so no model output can directly trigger a trade in this starter.

## Configuration and safety

`settings.toml` stores the ₹10,000 starting capital as 1,000,000 paise. Review all risk limits before running. This is a software starter, not a profitable strategy or a guarantee of fill realism. The paper broker approximates costs; it does not model exchange queues, taxes in full, option contract specifications, market impact, or outages. Current quote validation and the scaffold do not yet implement full NIFTY option-chain/five-minute candle normalization.

Keep `.env`, `data/paper.sqlite`, logs, and backups private. Rotate credentials if exposed. Use read-only market-data credentials. Never paste access tokens into logs, issue trackers, or source control. Android may suspend Termux in the background; disable battery optimization only if you need a continuous paper session, and expect disconnects. Resume by checking data freshness; stale input must not be traded.

## Azure static-IP migration (future)

Do not migrate for current paper testing. When you later move a reviewed paper system to an Azure VM, pin a stable outbound public IP if the provider requires allowlisting, keep market data and strategy credentials in a secret store, restrict inbound firewall access, add health monitoring and restart handling, and test reconnects. Static IP availability and cost depend on the current Azure plan/region. This repository still contains no live order API, so migration does not enable live trading.

## Android / Termux limits

Termux is a Linux-like environment rather than a conventional server. Rust and SQLite should be built natively in Termux; cross-compiling on Windows is not a substitute for validating on-device TLS, SQLite bundled C compilation, WebSocket TLS roots, battery behavior and reconnects. Dependency versions may need minor adjustment for a specific Termux/NDK toolchain. This environment did not have Rust/Cargo installed, so the included suite could not be compiled or run here.
