# INDmoney / INDstocks API v1 Reference

> **Safety & Implementation Guardrail**
>
> This repository implements **read-only market-data access only** for paper trading. **Zero live order, smart-order, portfolio, or account/margin endpoints are implemented in Rust.**
>
> `LIVE_ORDER_PLACEMENT_IMPLEMENTED` in `src/lib.rs` is hardcoded to `false` and enforced at runtime (`src/main.rs`, `src/paper_runner.rs`) and in automated tests (`tests/safety.rs`, `tests/e2e_trading_agents.rs`). Sections covering live order placement, portfolio, and margin APIs in Part III below are **documentation only** for post-trial evaluation.

**Authoritative Sources Cited**:
- Official Documentation Portal: `https://api-docs.indstocks.com` (`sitemap.xml`, 23 MkDocs pages, `lastmod: 2026-10-11`, `llms-full.txt`)
- Official OpenAPI 3.0.3 Specification: `https://api-docs.indstocks.com/openapi-spec.yaml` (`version: "1.0.0"`, server `https://api.indstocks.com`)
- Official Published Postman Collection: `https://documenter.getpostman.com/view/56363899/2sBY4LShyj` (including saved `200` / `400` / `401` / `403` examples)

---

## Table of Contents

1. [Part I — Authentication, Base URLs, Token Lifecycle, IP Rules, Rate Limits & Errors](#part-i--authentication-base-urls-token-lifecycle-ip-rules-rate-limits--errors)
2. [Part II — Paper-Trading Market Data APIs](#part-ii--paper-trading-market-data-apis)
3. [Part III — Post-Trial Live Trading APIs (Documentation Only — NOT Implemented in Rust)](#part-iii--post-trial-live-trading-apis-documentation-only--not-implemented-in-rust)
4. [Part IV — Post-Trial Live-Trading Readiness Checklist](#part-iv--post-trial-live-trading-readiness-checklist)

---

## Part I — Authentication, Base URLs, Token Lifecycle, IP Rules, Rate Limits & Errors

### 1.1 Base URLs & Transport Conventions

| Surface | Base URL | Payload Format / Notes |
|---|---|---|
| **REST API** | `https://api.indstocks.com` | JSON requests & responses (`GET /market/instruments` returns `text/csv`) |
| **Price Feed WebSocket** | `wss://ws-prices.indstocks.com/api/v1/ws/prices` | Text frames containing JSON strings |
| **Order Updates WebSocket** | `wss://ws-order-updates.indstocks.com/api/v1/ws/trades` | Text frames containing double-encoded JSON strings |

- **Timestamp Conventions**:
  - Default REST timestamps and WebSocket envelope `timestamp` fields are **Unix epoch milliseconds** (IST; `Websockets/` notes UTC on the order-updates envelope).
  - **Exception 1**: Historical candle `ts` fields are **Unix epoch seconds (IST)** representing candle open time.
  - **Exception 2**: Order book / trade book `created_at`, `updated_at`, and `trade_date` fields use ISO 8601 IST strings (`+05:30`, e.g., `2026-07-20T09:31:20+05:30`).

### 1.2 Authorization Header (Critical: NO `Bearer` Prefix)

All protected REST endpoints and both WebSocket handshakes require the **raw** access token in the `Authorization` header:

```http
Authorization: <INDSTOCKS_ACCESS_TOKEN>
```

> **CRITICAL WARNING — Do NOT send `Bearer`**
>
> The official `openapi-spec.yaml` (`AccessTokenAuth`: *"Raw access token — no `Bearer` prefix. Example header value is the token itself."*), `conventions/`, `getting-started/`, `MarketQuote/`, `utility/`, `Websockets/`, and the official Postman collection all mandate sending the **raw token** without a `Bearer ` prefix. Sending `Authorization: Bearer <token>` fails with `TokenException` (`401`/`403`).
>
> The only exception is the provisional `POST /generate/token` endpoint, which uses `x-api-key: <Client ID>` instead of `Authorization`.

#### Internal Official Documentation Discrepancies

The official docs portal contains minor legacy snippets on secondary pages that conflict with `openapi-spec.yaml` and Postman. Follow the authoritative specification:

| Doc Page | Legacy / Conflicting Text | Authoritative Behavior (`openapi-spec.yaml` + Postman) |
|---|---|---|
| `llm-metadata/` | Shows `Authorization: Bearer {token}` and `wss://api.indstocks.com/ws` | Send raw `Authorization: <token>` (no `Bearer`); Price WS is `wss://ws-prices.indstocks.com/api/v1/ws/prices` |
| `faq/` | Shows `params={'symbols': 'NSE:RELIANCE,...'}`, `wss://api.indstocks.com/ws` (`symbols`), and `API-Version: v1` | Use `scrip-codes=NSE_...` on REST; `wss://ws-prices.indstocks.com/api/v1/ws/prices` with `instruments` + `mode` |
| `api-overview/` | Lists `GET /trades/{order_id}` | Actual endpoint is `GET /order/trades` with JSON body `{"order_id", "segment"}` |
| `conventions/` | Generic snippet shows `"error_code": "INVALID_INPUT"` | Standard error envelopes use `"error_type"` (e.g., `"TokenException"`) |

### 1.3 Token Lifecycle

- **Token Expiry (TTL)**: **24 hours** from generation.
- **Revocation**: Revoking a token on the INDstocks dashboard or disabling TOTP immediately invalidates the active token (`TokenException`).

#### Method 1 — Manual Daily Dashboard Generation (Recommended for Paper Trading)

1. Log in at `https://indstocks.com/app/api-trading/access-tokens`.
2. Generate and copy the 24-hour `access_token`.
3. Store it in `.env` as `INDSTOCKS_ACCESS_TOKEN` using the repo helper scripts (preserving `GEMINI_API_KEY` and all other entries):
   - **Termux / Linux**: `bash scripts/update-indstocks-token.sh`
   - **Windows PowerShell**: `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\update-indstocks-token.ps1`

#### Method 2 — TOTP-Based Generation (`POST /generate/token`) `[PROVISIONAL]`

> **Provisional Status**: `openapi-spec.yaml` explicitly marks `POST /generate/token` as *"Provisional — not yet confirmed against a live deployment"*. Do not rely on it without live validation.

- **Enrollment (Web-Only)**: Enable TOTP in `https://indstocks.com/app/api-trading/access-tokens` (5-minute setup window, secret displayed once, generates static `Client ID` used as `x-api-key`). No API exists to enroll, reset, or disable TOTP.
- **Headers**:
  ```http
  x-api-key: <YOUR_CLIENT_ID>
  Content-Type: application/json
  ```
- **Request Body**:
  ```json
  {
    "mpin": "9828",
    "totp": "610446"
  }
  ```
- **Response `200` (Postman saved example — unconfirmed in `openapi-spec.yaml`)**:
  ```json
  {
    "token": "eyJh......",
    "expires_in": 1785461400
  }
  ```
  Note the field name is `token` (not `access_token`), and `expires_in` is an epoch-seconds timestamp in Postman's saved example.
- **Response `400` (Postman saved examples)**:
  - Wrong TOTP: `{"message": "Could not validate TOTP. Please try again."}`
  - Expired TOTP: `{"message": "Invalid TOTP code."}`
- **Operational Constraints**:
  - **Single active TOTP token**: Generating a new token immediately invalidates the previous TOTP-generated token.
  - **Throttle**: Maximum **1 request per 60 seconds**.
  - **Lockouts**: **5** wrong TOTP attempts within **15 minutes** triggers a **15-minute lockout**; **3** lockouts within **1 hour** triggers a **1-hour lockout** plus an email alert. Retrying during a lockout does not extend it, and an already-issued token continues working until its 24h expiry.

### 1.4 Static IP Whitelisting (`NSE/INVG/67858`)

- Configured at `https://www.indstocks.com/app/api-trading/access-tokens` with two slots: **Primary** and **Secondary** (both accept **IPv4 or IPv6** in any combination).
- **Scope**:
  - **Required ONLY for order placement, modification, and cancellation** (`/order`, `/order/modify`, `/order/cancel`, `/smart/order`, `/smart/order/modify`, `/smart/order/cancel`).
  - **NOT required** for read-only market data (`/market/option-chain`, `/market/quotes/*`, `/market/instruments*`, `/market/historical/*`, Price Feed WebSocket) or read-only account/order endpoints (`/user/profile`, `/funds`, `/order-book`, `/order`, `/order/trades`, `/trade-book`, `/portfolio/*`, `/margin`).
- **Update Rule**: Maximum **1 change per calendar week** per NSE circular; a populated slot cannot be cleared back to blank (only replaced with another valid IP).

### 1.5 Rate Limits

| Category | Endpoints Covered | Per Second | Per Minute | Per Day | Extra Limits / Notes |
|---|---|---:|---:|---:|---|
| **Order APIs** | `/order`, `/order/modify`, `/order/cancel`, `/smart/order`, `/smart/order/modify`, `/smart/order/cancel` | `10` | `—` | `—` | Max **25 modifications per order**. Sliced smart orders consume 1 unit per slice dispatched by background processor (max 10 slices/request). |
| **Data APIs** | `/market/instruments*`, `/market/historical/*`, `/market/option-chain` | `5` | `—` | `100,000` | Exceeding returns HTTP `429`. |
| **Quote APIs** | `/market/quotes/full`, `/market/quotes/ltp`, `/market/quotes/mkt` | `5` | `—` | `100,000` | Max `1000` scrip codes per request. |
| **Non-Trading APIs** | `/user/profile`, `/funds`, `/order-book`, `/order`, `/order/trades`, `/trade-book`, `/portfolio/*`, `/margin` | `15` | `—` | `100,000` | — |
| **Token Generation** | `POST /generate/token` | `—` | `1` | `—` | 5 wrong TOTPs / 15m -> 15m lockout; 3 lockouts / 1h -> 1h lockout. |
| **WebSocket Connections** | `ws-prices`, `ws-order-updates` | `—` | `—` | `—` | Max **3 active connections per user**. |
| **WebSocket Subscriptions** | `ws-prices` | `—` | `—` | `—` | Max **3,000 instruments per connection**. |

### 1.6 Error Envelopes & `error_type` Catalog

INDstocks uses **four distinct JSON error envelopes** depending on the endpoint family. Always inspect the HTTP status code first, then match across `error_type`, `debug_info`, `message`, or `error`.

#### Envelope 1 — Standard Error Envelope (Orders, Smart Orders, User, Funds, Portfolio, Margin)

```json
{
  "status": "error",
  "message": "Unable to validate user token",
  "error_type": "TokenException"
}
```

*(Note: `conventions/` contains a generic typo showing `"error_code": "INVALID_INPUT"`, whereas `errors/`, `openapi-spec.yaml`, and Postman all use `"error_type"`.)*

| `error_type` | HTTP Status | Meaning | Retryable? |
|---|---:|---|---|
| `InputException` | `400` | Malformed JSON, missing required fields, wrong types | No (fix request) |
| `RequestValidationException` | `400` | Well-formed request violates business, slicing, or RMS rule | No (fix params) |
| `DataException` | `400` | Invalid market/historical parameters (interval, scrip, range) | No (fix params) |
| `OrderException` | `400` | RMS rejection (`RMS: Margin exceeds ...`, etc.) | No |
| `TokenException` | `403` (Postman also `401`) | `access_token` invalid, expired, or revoked | Re-authenticate then retry |
| `UserException` | `403` | Account or segment (e.g., F&O / MCX) not activated | No |
| `NotFoundException` | `404` | Endpoint or resource does not exist | No |
| `MethodNotAllowedException` | `405` | Wrong HTTP verb | No |
| `Too Many Requests` | `429` | Rate limit exceeded | Yes (backoff) |
| `GeneralException` | `500` | Unexpected server error | Once / backoff |
| `NetworkException` | `503` | Upstream exchange or service error | Yes (backoff) |
| `ServiceUnavailableException` | `503` | Maintenance or overload | Yes (backoff) |
| `GatewayTimeoutException` | `504` | Upstream timeout | Yes (backoff) |
| `InternalServerException` | `512` | **`GET /order` or `GET /margin` failure** (including missing `order_id`!) | **No** (`512` on `GET /order` is terminal) |

#### Envelope 2 — Boolean `success: false` Envelope (`403` on Market Data / Quotes / Instruments; `400` Missing Auth on Option Chain)

- HTTP `403` (Postman `/market/instruments`, `/market/quotes/full`, `/market/quotes/ltp`, `/market/quotes/mkt`):
  ```json
  {
    "message": "The provided access_token is either incorrect, expired, or has been revoked. The user needs to re-authenticate.",
    "success": false
  }
  ```
- HTTP `400` (`utility/` Option Chain missing `Authorization` header):
  ```json
  {
    "message": "authorization not sent in request. Please try again.",
    "success": false
  }
  ```

#### Envelope 3 — `debug_info` Validation Envelope (`400` on Option Chain, Contracts, Instruments, Quotes, Historical)

- Option Chain invalid parameter or expiry format:
  ```json
  {
    "message": "Bad Request",
    "debug_info": "Invalid exchange, segment, underlying-scrip or expiry passed"
  }
  ```
- Quotes invalid or unquotable scrip code:
  ```json
  {
    "debug_info": "Invalid scrip codes or mode",
    "message": "Bad Request"
  }
  ```
- Instruments invalid `source`:
  ```json
  {
    "debug_info": "Source not present or invalid.",
    "message": "Bad Request"
  }
  ```
- Contracts (`segment=EQUITY` unsupported):
  ```json
  {
    "debug_info": "segment=EQUITY is not yet supported on this endpoint; use segment=DERIVATIVE",
    "message": "Not Supported"
  }
  ```
- Historical validation examples:
  - `{"debug_info": "Invalid scrip codes", "message": "Bad Request"}` (>5 scrips)
  - `{"debug_info": "trading-symbols must contain 1-50 symbols.", "message": "Bad request"}`
  - `{"debug_info": "invalid interval.", "message": "Bad request"}`
  - `{"debug_info": "start_time must not be after end_time.", "message": "Bad request"}`

#### Envelope 4 — Option Chain Rate-Limit Envelope (`429`)

```json
{
  "error": "Rate limit exceeded",
  "success": false
}
```

---

## Part II — Paper-Trading Market Data APIs

### Verified Official Specs vs. Unverified Gaps Summary

| Surface / Feature | Status | Details |
|---|---|---|
| `GET /market/instruments/expiries` | **Verified Official Spec** | Ascending `YYYY-MM-DD` NIFTY expiries |
| `GET /market/option-chain` | **Verified Official Spec** | Returns live NIFTY spot (`data.underlying_ltp`) + full CE/PE strikes with bid/ask, OI, IV, Greeks |
| `GET /market/quotes/{ltp,full,mkt}` (`NFO_`, `NSE_`, `BSE_`, `MCX_`) | **Verified Official Spec** | Batch up to 1000 scrips; single bad scrip fails entire call with `400` |
| `GET /market/instruments?source=...` & `/search` & `/expired/*` | **Verified Official Spec** | CSV master (`index` 3-col, `fno`/`mcx` 23-col, `equity` 26-col) & paginated contract search |
| `GET /market/historical/{interval}` & `/expired/{interval}` | **Verified Official Spec** | 15 intervals; epoch-ms query, epoch-sec `ts` open time, `"success": true` |
| `market_depth` flat vs scrip-nested JSON | **Documented Discrepancy** | `MarketQuote/` shows `market_depth.depth`; Postman saved `200` shows `market_depth.<scrip>.depth` + Indian comma strings |
| REST index spot via `/market/quotes/*` (e.g., `NIDX_40000001`) | **Documented Gap (Unverified)** | Official REST quote docs list only `NSE_`, `BSE_`, `NFO_`, `MCX_` — no index prefix. Use `GET /market/option-chain` `underlying_ltp` for NIFTY spot until live-tested |
| WebSocket NIFTY 50 index token (`40000001` vs `NIDX:26000`) | **Documented Caveat (Unverified Live)** | `glossary/` warns REST `source=index` (`40000001`) and WS (`NIDX:26000` example) may not share the same token; verify live before streaming index spot |
| WebSocket `quote` mode on `NFO:` options | **Unverified Live Gap** | Postman `quote` example is equity `NSE:2885` (OHLCV only, no bid/ask depth); verify live before relying on WS `quote` mode for `NFO:` |

---

### 2.1 Upcoming Expiries (`GET /market/instruments/expiries`)

Discover upcoming NIFTY expiry dates before requesting the option chain.

- **Method & Path**: `GET https://api.indstocks.com/market/instruments/expiries`
- **Headers**: `Authorization: <INDSTOCKS_ACCESS_TOKEN>`
- **Query Parameters**:
  - `underlying` (string, required): e.g. `NIFTY`
  - `segment` (string, required): `[DERIVATIVE]` (`EQUITY` returns `400`)
- **Example Request**:
  ```http
  GET /market/instruments/expiries?underlying=NIFTY&segment=DERIVATIVE HTTP/1.1
  Host: api.indstocks.com
  Authorization: <INDSTOCKS_ACCESS_TOKEN>
  ```
- **Response `200`** (dates in ascending `YYYY-MM-DD` order):
  ```json
  {
    "status": "success",
    "data": ["2026-08-25", "2026-09-01", "2026-09-08"]
  }
  ```
- **Error `400`**: `DebugInfoErrorResponse` (`{"message": "Bad Request", "debug_info": "..."}` or `"Not Supported"`).

---

### 2.2 Option Chain with Greeks (`GET /market/option-chain`)

Primary **100%-officially-documented** endpoint for live-data NIFTY paper trading: returns both `underlying_ltp` (NIFTY 50 spot) and the CE/PE strike ladder in a single request.

- **Method & Path**: `GET https://api.indstocks.com/market/option-chain`
- **Headers**: `Authorization: <INDSTOCKS_ACCESS_TOKEN>`
- **Query Parameters**:

  | Param | Type | Required | Enum / Format | Notes |
  |---|---|---|---|---|
  | `exchange` | string | Yes | `NSE`, `BSE` | Exchange of the option contracts |
  | `segment` | string | Yes | `INDEX`, `EQUITY` | **Underlying's** segment — **do NOT pass `DERIVATIVE`** |
  | `underlying-scrip` | string | Yes | Bare `SECURITY_ID` | `"40000001"` for `NIFTY 50` (from `source=index`); `"2885"` for `RELIANCE` (from `source=equity`) |
  | `expiry` | string | Yes | `YYYY-MM-DD` | From `/market/instruments/expiries` |
  | `strike_count` | integer | No | Default `10` | Strikes **per side** of ATM -> returns `(2 * strike_count) + 1` strikes |

- **Example Request**:
  ```http
  GET /market/option-chain?exchange=NSE&segment=INDEX&underlying-scrip=40000001&expiry=2026-08-18&strike_count=10 HTTP/1.1
  Host: api.indstocks.com
  Authorization: <INDSTOCKS_ACCESS_TOKEN>
  ```
- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": {
      "underlying_ltp": 24471.7,
      "expiry": "2026-08-18",
      "strikes": {
        "24450": {
          "ce": {
            "security_id": "45108",
            "trading_symbol": "NIFTY-Aug2026-24450-CE",
            "last_price": 167.9,
            "previous_close_price": 274.95,
            "oi": 1608490,
            "previous_oi": 1606988,
            "volume": 9079330,
            "top_bid_price": 166.05,
            "top_bid_quantity": 195,
            "top_ask_price": 167.5,
            "top_ask_quantity": 130,
            "iv": 10.5,
            "greeks": {
              "delta": 0.56,
              "gamma": 0.0011,
              "theta": -10.04,
              "vega": 13.39
            }
          },
          "pe": {
            "security_id": "45109",
            "trading_symbol": "NIFTY-Aug2026-24450-PE",
            "last_price": 117.65,
            "previous_close_price": 88.9,
            "oi": 1579370,
            "previous_oi": 1579129,
            "volume": 11974495,
            "top_bid_price": 117.65,
            "top_bid_quantity": 195,
            "top_ask_price": 119.4,
            "top_ask_quantity": 195,
            "iv": 10.4,
            "greeks": {
              "delta": -0.44,
              "gamma": 0.0011,
              "theta": -9.95,
              "vega": 13.39
            }
          }
        }
      }
    }
  }
  ```
- **Schema Notes & Omitted Fields**:
  - `data.strikes` keys are strike price strings (e.g., `"24450"`); **key order is not guaranteed** — parse and sort numerically.
  - `ce` / `pe` field types: `security_id` (`string`), `trading_symbol` (`string`), `last_price` (`number`), `previous_close_price` (`number`), `oi` (`int64`), `previous_oi` (`int64`), `volume` (`int64`), `top_bid_price` (`number`), `top_bid_quantity` (`int64`), `top_ask_price` (`number`), `top_ask_quantity` (`int64`), `iv` (`number` percentage, sibling of `greeks`), `greeks.{delta, gamma, theta, vega}` (`number`).
  - **Omitted fields**: No `rho` in `greeks`, no `oi_change` (compute `oi - previous_oi`), no `pcr`, and no `lot_size` / `tick_size` (obtain those from `/market/instruments?source=fno` or `/market/instruments/search`). Only top-of-book bid/ask (level 1) is returned.
- **Exact Errors**:
  - `400`: `{"message": "Bad Request", "debug_info": "Invalid exchange, segment, underlying-scrip or expiry passed"}`
  - `400` (missing `Authorization`): `{"message": "authorization not sent in request. Please try again.", "success": false}`
  - `429`: `{"error": "Rate limit exceeded", "success": false}`

---

### 2.3 Market Quotes (`GET /market/quotes/ltp`, `/full`, `/mkt`)

- **Method & Paths**:
  - `GET https://api.indstocks.com/market/quotes/ltp?scrip-codes=...`
  - `GET https://api.indstocks.com/market/quotes/full?scrip-codes=...`
  - `GET https://api.indstocks.com/market/quotes/mkt?scrip-codes=...`
- **Headers**: `Authorization: <INDSTOCKS_ACCESS_TOKEN>`
- **Query Parameter**: `scrip-codes` (required, comma-separated `SEGMENT_TOKEN`, up to **1000** scrips per request; mixed segments allowed).
- **Documented Prefixes**: `NSE_` (NSE cash), `BSE_` (BSE cash), `NFO_` (NSE F&O, e.g. `NFO_51011`), `MCX_` (MCX commodity F&O).
- **CRITICAL BATCH FAILURE RULE**: Every code in `scrip-codes` must resolve. **A single invalid or unquotable scrip code fails the entire batch request** with HTTP `400`:
  ```json
  {
    "debug_info": "Invalid scrip codes or mode",
    "message": "Bad Request"
  }
  ```
- **Auth Error (`403`, Postman)**:
  ```json
  {
    "message": "The provided access_token is either incorrect, expired, or has been revoked. The user needs to re-authenticate.",
    "success": false
  }
  ```

#### `GET /market/quotes/ltp` (`200`)

```json
{
  "status": "success",
  "data": {
    "NSE_3045": {
      "live_price": 792.5
    }
  }
}
```

#### `GET /market/quotes/full` (`200`)

```json
{
  "status": "success",
  "data": {
    "NSE_3045": {
      "live_price": 788.8,
      "day_change": -3.5,
      "day_change_percentage": -0.44,
      "day_low": 788.35,
      "day_high": 795.5,
      "day_open": 792.5,
      "prev_close": 792.3,
      "52week_high": 899,
      "52week_low": 680,
      "upper_circuit": 871.5,
      "lower_circuit": 713.1,
      "market_depth": { "...": "..." },
      "volume": 3546732
    }
  }
}
```
*(For `NFO_` and `MCX_` contracts, `MarketQuote/` also documents `open_interest` and `open_interest_change`.)*

#### `GET /market/quotes/mkt` (`200`) — Flat vs. Nested `market_depth` & Comma-String Quirk

- **Docs (`MarketQuote/`) flat layout**:
  ```json
  {
    "status": "success",
    "data": {
      "NSE_3045": {
        "market_depth": {
          "aggregate": {
            "total_buy": "5,82,909",
            "total_sell": "11,01,938",
            "buy_percentage": 34.6,
            "sell_percentage": 65.4
          },
          "depth": [
            {
              "buy": { "quantity": "6.00", "price": "788.95" },
              "sell": { "quantity": "21.00", "price": "789.00" }
            }
          ]
        }
      }
    }
  }
  ```
- **Postman saved `200` layout (both `/full` and `/mkt`)** nests an extra scrip-code key inside `market_depth`:
  ```json
  {
    "status": "success",
    "data": {
      "NSE_1594": {
        "market_depth": {
          "NSE_1594": {
            "aggregate": {
              "total_buy": "0.00",
              "total_sell": "1,281",
              "buy_percentage": 0,
              "sell_percentage": 100
            },
            "depth": [
              {
                "buy": { "quantity": "0.00", "price": "0.00" },
                "sell": { "quantity": "1,281", "price": "1,102.60" }
              }
            ]
          }
        }
      }
    }
  }
  ```
- **Parser Requirements**:
  1. Accept both flat (`market_depth.depth`) and scrip-nested (`market_depth.<scrip_code>.depth`).
  2. Strip commas (including Indian grouping such as `"5,82,909"`) before parsing string `quantity`, `price`, `total_buy`, and `total_sell` values as numbers.
- **Documented Gap — NIFTY 50 Spot Prefix (`NIDX_`)**: Official docs and `openapi-spec.yaml` do **not** document an index prefix (such as `NIDX_`) for `/market/quotes/*`. Do not assume `NIDX_40000001` works on `/market/quotes/*` without a live smoke test; use `GET /market/option-chain` (`data.underlying_ltp`) as the authoritative NIFTY 50 spot source.

---

### 2.4 Instruments Master CSV, Contract Search & Expired Contracts

#### 1. `GET /market/instruments?source={equity|fno|index|mcx}`

- **Response Content-Type**: `text/csv` (Note: `openapi-spec.yaml` enum lists `equity`, `fno`, `index`, while `instruments/` docs also document `mcx`).
- **`source=index` (3 positional columns)**:
  ```csv
  EXCH,SEGMENT,SECURITY_ID
  NSE,NIFTY 50,40000001
  NSE,NIFTY IT,40000004
  BSE,BSE Focused IT,40000129
  ```
  *Quirk*: Column 2 is header-named `SEGMENT` but actually contains the **index name** (`NIFTY 50`). Parse `source=index` positionally.
- **`source=fno` and `source=mcx` (23 columns in `instruments/`)**:
  `EXCH, SEGMENT, SECURITY_ID, INSTRUMENT_NAME, EXPIRY_CODE, TRADING_SYMBOL, LOT_UNITS, CUSTOM_SYMBOL, EXPIRY_DATE, STRIKE_PRICE, OPTION_TYPE, TICK_SIZE, EXPIRY_FLAG, SEM_EXCH_INSTRUMENT_TYPE, SERIES, SYMBOL_NAME, UNDERLYING_SCRIP_NAME, FREEZE_QTY, UPPER_LIMIT, LOWER_LIMIT, ISIN, GENERAL_FACTOR, DELIVERY_UNIT`
  *(On `mcx`, `UPPER_LIMIT`/`LOWER_LIMIT` are percentages rather than absolute prices.)*
- **`source=equity` (26 columns in `instruments/`)**:
  Same 23 columns plus `INTRADAY_LEVERAGE, HAIRCUT_PERCENTAGE, PLEDGE_ELIGIBLE`.
- **Postman Discrepancy**: Postman's saved CSV examples show only the first 16 columns (`EXCH` through `SYMBOL_NAME`). Parse `fno`/`mcx`/`equity` by header name rather than asserting a fixed column count.

#### 2. `GET /market/instruments/search`

- **Query Parameters**:
  - `underlying` (required, e.g., `NIFTY`)
  - `segment` (required, `DERIVATIVE` only — `EQUITY` returns `400`)
  - `instrument_type` (optional: `OPTIDX` | `OPTSTK` | `FUTIDX` | `FUTSTK`)
  - `expiry` (optional, `YYYY-MM-DD`)
  - `strike_from`, `strike_to` (optional numbers)
  - `option_type` (optional: `CE` | `PE`)
  - `page` (optional int, default `1`), `page_size` (optional int, default `50`, max `100`)
- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": {
      "count": 804,
      "page": 1,
      "page_size": 50,
      "instruments": [
        {
          "security_id": "58072",
          "trading_symbol": "NIFTY26AUG25FUT",
          "expiry": "2026-08-25",
          "strike_price": null,
          "option_type": null,
          "lot_size": 75
        }
      ]
    }
  }
  ```

#### 3. Expired Instruments Endpoints

- `GET /market/instruments/expired/search`: Same parameters as `/search` plus mandatory `expiry_from` and `expiry_to` (`YYYY-MM-DD`, max **5 years**). Response omits `security_id` (contracts are keyed by `trading_symbol`).
- `GET /market/instruments/expired/expiries`: `underlying`, `segment=DERIVATIVE`, `expiry_from`, `expiry_to` (strict **1-year** cap). Returns `data: ["YYYY-MM-DD", ...]` in **descending** order (reverse of live `/expiries`).
- `GET /market/instruments/expired/contracts`: `underlying`, `segment=DERIVATIVE`, `expiry` (required, from `/expired/expiries`), `instrument_type` (optional). Returns an unpaginated flat array `data: [{trading_symbol, instrument_type, strike_price, option_type, lot_size, expiry}]`.

---

### 2.5 Historical Candles (`GET /market/historical/{interval}` & `/expired/{interval}`)

- **Supported `interval` Values & Per-Call Max Range**:
  - **7 days max**: `1minute`, `2minute`, `3minute`, `4minute`, `5minute`, `10minute`, `15minute`, `30minute`
  - **15 days max**: `60minute`, `120minute`, `180minute`, `240minute`
  - **1 year max**: `1day`, `1week`, `1month`
- **Live Endpoint Query Params** (`GET /market/historical/{interval}`):
  - `scrip-codes` (required, max **5** `SEGMENT_TOKEN`, e.g., `NSE_3045,NFO_51011`; >5 returns `400` `{"debug_info": "Invalid scrip codes", "message": "Bad Request"}`)
  - `start_time` (required `int64`): Unix epoch **milliseconds (IST)**, inclusive
  - `end_time` (required `int64`): Unix epoch **milliseconds (IST)**, exclusive
- **Expired Endpoint Query Params** (`GET /market/historical/expired/{interval}`):
  - `trading-symbols` (required, **1–50** comma-separated `trading_symbol`s, e.g., `NIFTY26AUG2523950CE`)
  - `segment` (optional: `NFO` [default] | `BFO`)
  - `start_time`, `end_time` (required `int64`, epoch **milliseconds (IST)**)
- **Response `200`** (**Note `"success": true`** — NOT `"status": "success"`!):
  ```json
  {
    "success": true,
    "data": {
      "NSE_1594": {
        "candles": [
          { "ts": 1782877500, "o": 1007, "h": 1013.9, "l": 999.3, "c": 1000.4, "v": 2847163 },
          { "ts": 1782881100, "o": 1000.4, "h": 1002, "l": 996.7, "c": 999.6, "v": 1389042 }
        ]
      }
    }
  }
  ```
- **Critical Quirks & Edge Cases**:
  - `start_time` / `end_time` query parameters are **epoch milliseconds (IST)**, whereas each candle `ts` is **Unix epoch seconds (IST)** representing the candle **open** time `[ts, ts + interval)` anchored to **`09:15 IST`** (e.g., `60minute` bars open at `09:15`, `10:15`, etc.).
  - Requests exceeding the max window return **`200` clamped** to the allowed range (not an error), with no pagination cursor (page manually by timestamp window).
  - Scrips/symbols with no data or unknown expired symbols are **omitted** from `data`.
  - A zero-width window (`start_time == end_time`) returns `"candles": null`.

---

### 2.6 Price Feed WebSocket (`wss://ws-prices.indstocks.com/api/v1/ws/prices`)

- **URL**: `wss://ws-prices.indstocks.com/api/v1/ws/prices`
- **Handshake Header**: `Authorization: <INDSTOCKS_ACCESS_TOKEN>` (raw token, no `Bearer` prefix).
- **Limits**: Max `3` active connections/user, max `3,000` instruments/connection.
- **Subscribe / Unsubscribe Client Frame**:
  ```json
  {
    "action": "subscribe",
    "mode": "ltp",
    "instruments": ["NSE:2885", "NFO:51011", "NIDX:26000"]
  }
  ```
  - `action`: `"subscribe"` | `"unsubscribe"`
  - `mode`: `"ltp"` | `"quote"`
  - `instruments`: Colon-separated `SEGMENT:TOKEN` (`NSE:`, `BSE:`, `NFO:`, `BFO:`, `NIDX:`, `BIDX:`).
- **Server Text Frames (JSON strings)**:
  - `ltp` mode:
    ```json
    {
      "mode": "ltp",
      "instrument": "2885",
      "timestamp": 1750138351089,
      "data": { "ltp": 1426 }
    }
    ```
  - `quote` mode (Postman saved example — OHLCV only, **no bid/ask market depth**):
    ```json
    {
      "mode": "quote",
      "instrument": "2885",
      "timestamp": 1784185315958,
      "data": {
        "open": 1295.5,
        "high": 1309.4,
        "low": 1295.5,
        "close": 1305.4,
        "volume": 6228599
      }
    }
    ```
  - The server also sends periodic heartbeat frames that clients should ignore.
- **CRITICAL TOKEN CAVEAT (`glossary/`)**:
  `source=index` lists `NIFTY 50` as `40000001` (used for `/market/option-chain`'s `underlying-scrip`), whereas `Websockets/` uses `NIDX:26000`. Official `glossary/` explicitly warns: *"the two surfaces may not share the same index token. Do not carry a token from one to the other — look it up per surface."* Because no documented `/market/instruments` column lists WebSocket `NIDX:` tokens, verify live before streaming NIFTY spot over WebSocket. Also verify live whether `quote` mode emits frames for `NFO:` option contracts.

---

## Part III — Post-Trial Live Trading APIs (Documentation Only — NOT Implemented in Rust)

> **Reminder**: None of the endpoints in Part III are implemented in Rust (`LIVE_ORDER_PLACEMENT_IMPLEMENTED = false`). Order placement/modification/cancellation also requires Static IP Whitelisting (`NSE/INVG/67858`) on `https://www.indstocks.com/app/api-trading/access-tokens`.

### 3.1 Standard Orders (`normal_orders/`)

#### 1. `POST /order` (Requires Static IP Whitelisting)

- **Request Body (`OrderRequest`)**:

  | Field | Type | Required | Enum / Constraints |
  |---|---|---|---|
  | `txn_type` | string | Yes | `BUY`, `SELL` |
  | `exchange` | string | Yes | `NSE`, `BSE`, `MCX` (`MCX` only with `COMMODITY`) |
  | `segment` | string | Yes | `EQUITY`, `DERIVATIVE`, `COMMODITY` |
  | `product` | string | Yes | `EQUITY`: `CNC`, `INTRADAY`; `DERIVATIVE`/`COMMODITY`: `MARGIN`, `INTRADAY` |
  | `order_type` | string | Yes | `LIMIT`, `MARKET` (NO `SL`/`SL-M` — use `/smart/order` for trigger/SL/target; `MARKET` is converted server-side to `LIMIT` with a protection band) |
  | `validity` | string | Yes | `DAY` only (`IOC` rejected) |
  | `security_id` | string | Yes | Bare `SECURITY_ID` string (no prefix), e.g., `"45108"` |
  | `qty` | integer | Yes | `>= 1` in **units** (NOT lots); must be exact multiple of `LOT_UNITS` and `< FREEZE_QTY` (from `/market/instruments?source=fno`) |
  | `limit_price` | number | Cond. | Required for `LIMIT`. Omit for `MARKET` (`COMMODITY` `MARKET` with `limit_price` is rejected) |
  | `algo_id` | string | No | Defaults `"99999"` (NSE/MCX), `"9999999999999999"` (BSE) |
  | `is_amo` | boolean | No | Default `false` (ignored for `COMMODITY`) |
  | `remarks` | string | No | Correlation tag, max `100` chars (silently truncated), echoed on `/order`, `/order-book`, `/trade-book`; immutable on modify; `"TV-TERMINAL"` rejected |

- **Example Request**:
  ```json
  {
    "txn_type": "BUY",
    "exchange": "NSE",
    "segment": "DERIVATIVE",
    "product": "MARGIN",
    "order_type": "LIMIT",
    "validity": "DAY",
    "security_id": "45108",
    "qty": 75,
    "limit_price": 167.5,
    "is_amo": false,
    "remarks": "nifty-paper-trial-post"
  }
  ```
- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": {
      "order_id": "DRV-29301125",
      "order_status": "INITIATED"
    }
  }
  ```
  *(RMS or exchange rejection can return `200` with `"order_status": "FAILED"` and `"reason": "..."`, or `400` `RequestValidationException` / `OrderException`.)*
- **Order ID Prefixes**: `EQ-` (equity), `DRV-` (derivative), `COM-` (commodity), `GTT-` (smart-order legs / trigger / out-of-band parent).
- **All 15 `order_status` Values**:
  `QUEUED`, `O-PENDING`, `SL-PENDING`, `PROCESSING`, `ABORTED`, `INITIATED`, `SUCCESS`, `CANCELLED`, `MODIFIED`, `PENDING`, `EXPIRED`, `FAILED`, `"PARTIALLY FILLED"`, `"PARTIALLY FILLED - CANCELLED"`, `"PARTIALLY FILLED - EXPIRED"`.

#### 2. `POST /order/modify` (Requires Static IP Whitelisting)

- **Request Body**: `{"order_id": "DRV-2049", "segment": "DERIVATIVE", "qty": 75, "limit_price": 168.0}` (all 4 required; max **25 modifications per order**; `remarks` not accepted).
- **Response `200`**: `{"status": "success", "data": {"order_id": "DRV-2049", "order_status": "MODIFIED"}}`.

#### 3. `POST /order/cancel` (Requires Static IP Whitelisting)

- **Request Body**: `{"order_id": "DRV-2049", "segment": "DERIVATIVE"}`.
- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": {
      "order_id": "DRV-2049",
      "order_status": "CANCELLED",
      "child_order_details": {
        "order_id": "GTT-91822",
        "order_status": "CANCELLED"
      }
    }
  }
  ```

#### 4. `GET /order-book`

- No query/body params. Returns today's orders array (`data[]`) with fields:
  `created_at`, `updated_at`, `user_id`, `security_id`, `isin`, `name`, `id`, `exch_order_id`, `txn_type`, `exchange`, `segment`, `product`, `order_type` (incl. `"OCO"`), `validity`, `mkt_type`, `off_mkt_flag`, `traded_qty` (int), `requested_qty` (int), `requested_price` (string), `traded_price` (string), `sl_trigger_price` (string), `sl_limit_price` (string), `tgt_trigger_price` (string), `tgt_limit_price` (string), `status`, `extra_info`, `remarks`, `child_order_id`, `parent_order_id`, `is_tsl`, `tsl_step_size`.

#### 5. `GET /order` (`GET`-with-Body + HTTP `512` Quirks)

- **QUIRK 1**: Sends a JSON body on an HTTP `GET` request:
  ```json
  { "order_id": "DRV-27373858", "segment": "DERIVATIVE" }
  ```
- **QUIRK 2**: Returns **HTTP `512`** (`{"status": "error", "message": "Unable to fetch order details", "error_type": "InternalServerException"}`) — **NOT `404`** — when an order does not exist or cannot be retrieved. Do **not** treat `512` as retryable.
- **Response `200`**: `{"status": "success", "data": { ...single OrderDetails object... }}`.

#### 6. `GET /order/trades` (`GET`-with-Body Quirk)

- **QUIRK**: Sends a JSON body on HTTP `GET`: `{"order_id": "DRV-85322703", "segment": "DERIVATIVE"}`.
- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": [
      {
        "fill_id": 1279916,
        "exch_order_id": "1100000017281712",
        "quantity": 65,
        "price": 77.8,
        "trade_date": "2026-07-20T09:31:20+05:30"
      }
    ]
  }
  ```

#### 7. `GET /trade-book?segment={EQUITY|DERIVATIVE|COMMODITY}`

- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": [
      {
        "fill_id": 1020280,
        "exch_order_id": "2400000124991381",
        "quantity": 2425,
        "price": 1.55,
        "trade_date": "2025-11-11T17:48:23+05:30",
        "trade_serial_no": "17628437030186581215",
        "scrip_code": "99133",
        "remarks": "momentum-v2/sig-4471"
      }
    ]
  }
  ```
  *(Note: `api-overview/` lists `GET /trades/{order_id}`, which is stale; `normal_orders/`, `openapi-spec.yaml`, and Postman all specify `GET /order/trades`.)*

---

### 3.2 Smart Orders / GTT (`smart_orders/`)

#### 1. `POST /smart/order` (Requires Static IP Whitelisting)

- **Required Fields**: `txn_type` (`BUY`|`SELL`), `exchange` (`NSE`|`MCX` — `BSE` is not in `SmartOrderRequest.exchange`), `segment` (`EQUITY`|`DERIVATIVE`|`COMMODITY`), `product`, `order_type` (`LIMIT`|`MARKET`|`TRIGGER`), `validity` (`DAY`), `security_id`, `qty`.
- **Optional / Conditional Fields**:
  - `algo_id` (default `"99999"`)
  - `limit_price` (required for `LIMIT`; omit for `MARKET`; **REJECTED (`400`) when `order_type == "TRIGGER"`** — use `trigger_limit_price`)
  - `trigger_price` (required for `TRIGGER`; tick-size multiple; `BUY` > CMP, `SELL` < CMP)
  - `trigger_limit_price` (optional for `TRIGGER`; defaults to `trigger_price`)
  - **Stop-Loss Leg**: `sl_trigger_price` + `sl_limit_price` (**both required together**; `BUY`: `sl_limit <= sl_trigger < entry_price`; `SELL`: `sl_limit >= sl_trigger > entry_price`)
  - **Target Leg**: `tgt_trigger_price` + `tgt_limit_price` (**both required together**; `BUY`: `tgt_limit >= tgt_trigger > entry_price`; `SELL`: `tgt_limit <= tgt_trigger < entry_price`)
  - `slice_order` (`boolean`, default `false`; `DERIVATIVE`/`COMMODITY` only — rejected on `EQUITY`; splits `qty > FREEZE_QTY` [or > ₹3.5 Cr on MCX] into whole-lot slices up to **10 slices max**, >10 slices -> `400`)
  - `remarks` (max 100 chars, carried onto all legs & slices)
- **CRITICAL CAVEAT — `is_tsl` & `tsl_step_size` Are Silently Ignored**:
  Official `smart_orders/`, `changelog/`, and `openapi-spec.yaml` explicitly state that **`is_tsl` and `tsl_step_size` are NOT YET LIVE**: they are accepted without error and **silently ignored**, placing an ordinary non-trailing stop-loss. Post-trial live trading **must still implement trailing stop-loss logic client-side** (modifying the `GTT-` SL leg via `POST /smart/order/modify` or exiting via opposing `POST /order`).
- **Response `200` (Unsliced)**:
  ```json
  {
    "status": "success",
    "data": {
      "order_data": [
        {
          "order_id": "DRV-28131451",
          "order_status": "CREATED",
          "child_order_details": {
            "order_id": "GTT-2914581",
            "order_status": "CREATED"
          }
        }
      ]
    }
  }
  ```
- **Response `200` (Sliced with `slice_order: true`)**:
  ```json
  {
    "status": "success",
    "data": {
      "order_data": [
        { "order_id": "DRV-28131451", "order_status": "INITIATED" },
        { "order_id": "DRV-28131452", "order_status": "INITIATED" }
      ]
    }
  }
  ```
  (`child_order_details` is omitted for async `DRV-` slices; poll `GET /order` or `GET /order-book` for `child_order_id`.)

#### 2. `POST /smart/order/modify` (Requires Static IP Whitelisting)

- **Request Body**: Required: `order_id`, `segment`, `algo_id`. Optional: `qty`, `limit_price`, `trigger_price`, `trigger_limit_price`, `sl_trigger_price`, `sl_limit_price`, `tgt_trigger_price`, `tgt_limit_price`.
- **Quirks**: `order_type` is ignored; order type is inferred from the price fields sent and must match the existing order. Modifying an OCO leg pair requires both `sl_trigger_price` and `tgt_trigger_price`. `is_tsl` / `tsl_step_size` / `remarks` are not accepted.
- **Response `200`**: `{"status": "success", "data": {"order_id": "DRV-123", "order_status": "MODIFIED"}}`.

#### 3. `POST /smart/order/cancel` (Requires Static IP Whitelisting)

- **Request Body**: `{"order_id": "GTT-2914581", "segment": "DERIVATIVE"}` (cancelling a parent also cancels its linked `GTT-` child leg and returns `child_order_details`).
- **Response `200`**: `{"status": "success", "data": {"order_id": "GTT-2914581", "order_status": "CANCELLED"}}`.

---

### 3.3 Portfolio, Positions & Square-Off (`portfolio_funds/`)

#### 1. `GET /portfolio/holdings`

- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": [
      {
        "security_id": "18520",
        "symbol": "CUPID",
        "isin": "INE509F01029",
        "total_qty": 1,
        "used_qty": 0,
        "avg_price": 217.3,
        "t1_qty": 1,
        "t1_avg_price": 217.3,
        "dp_qty": 0,
        "dp_avg_price": 0
      }
    ]
  }
  ```
  *(Contains no live valuation or P&L fields — pair with `/market/quotes/ltp` or `/full`.)*

#### 2. `GET /portfolio/positions`

- **Optional Query Params** (lowercase / case-insensitive, e.g., `?segment=derivative&product=margin`):
  - `segment`: `equity` | `derivative` | `commodity`
  - `product`: `cnc` | `intraday` (equity); `margin` | `intraday` (derivative/commodity). **Sending `product` without `segment` returns HTTP `400`.**
- **Response `200` (Derivative example)**:
  ```json
  {
    "status": "success",
    "data": [
      {
        "position_id": "535654528",
        "security_id": "823580",
        "symbol": "SENSEX",
        "segment": "DERIVATIVE",
        "product": "MARGIN",
        "exchange": "",
        "drv_instrument": "OPTIDX",
        "drv_expiry_date": "07/16/2026 14:00",
        "drv_option_type": "CE",
        "drv_strike_price": 82000,
        "net_qty": 0,
        "avg_price": 1.2,
        "buy_qty": 20,
        "buy_avg": 1.25,
        "sell_qty": 20,
        "sell_avg": 1.2,
        "realized_profit": -1.0,
        "day_buy_qty": null,
        "day_buy_val": null,
        "day_sell_qty": null,
        "day_sell_val": null,
        "cf_buy_qty": null,
        "cf_buy_val": null,
        "cf_sell_qty": null,
        "cf_sell_val": null
      }
    ]
  }
  ```
  *(Note: `exchange` may be `""` in derivative responses; equity rows include `isin` and omit `drv_*` fields.)*

#### 3. Position Square-Off Pattern (No Dedicated Exit Endpoint)

**INDstocks has NO dedicated `/positions/exit` or `/square-off` endpoint.** To square off live positions:
1. Poll `GET /portfolio/positions?segment=derivative` and filter `net_qty != 0`.
2. Cancel any open protective `GTT-` child orders first via `POST /smart/order/cancel` (check `GET /order-book` for active `GTT-` orders on the same `security_id`).
3. Place an opposing `POST /order` (`txn_type: "SELL"` if `net_qty > 0`, `"BUY"` if `net_qty < 0`) using the position's `segment`, `product`, `security_id`, `exchange` (default `"NSE"` for NIFTY `DERIVATIVE` when `exchange` is `""`), `order_type: "MARKET"` (or `"LIMIT"`), `validity: "DAY"`, and `qty: abs(net_qty)` (or `POST /smart/order` with `slice_order: true` if `abs(net_qty) >= FREEZE_QTY`).

---

### 3.4 Account, Funds, Margin & Order Updates WebSocket

#### 1. `GET /user/profile`

```json
{
  "status": "success",
  "data": {
    "user_id": "1234567",
    "email": "john.doe@example.com",
    "first_name": "John",
    "last_name": "Doe",
    "demat_id": "",
    "is_nse_onboarded": true,
    "is_bse_onboarded": true,
    "is_nse_fno_onboarded": true,
    "is_bse_fno_onboarded": true,
    "ucc": "1ABCDE2N7X",
    "is_ddpi_active": true
  }
}
```

#### 2. `GET /funds`

```json
{
  "status": "success",
  "data": {
    "sod_balance": 4996.47,
    "pledge_received": 0,
    "pledge_remained": 0,
    "detailed_avl_balance": {
      "option_sell": 2980.40,
      "future": 2980.40,
      "option_buy": 4449.65,
      "comm_option_buy": 2980.40,
      "eq_mis": 2980.40,
      "eq_cnc": 2980.40,
      "eq_mtf": 2980.40
    },
    "withdrawal_balance": 2983.47,
    "funds_added": 0,
    "funds_withdrawn": 0,
    "realized_pnl": -751.92,
    "unrealized_pnl": 62.15,
    "brokerage": 0,
    "eq_charges": 0,
    "fno_charges": 0
  }
}
```

#### 3. `GET /margin` (`GET`-with-Body + camelCase + String Numbers Quirks)

- **QUIRK 1**: Uses HTTP `GET` with a JSON request body.
- **QUIRK 2**: Uses **camelCase** keys (`securityID`, `txnType`) and **string** `quantity` / `price` (all required; `price: ""` or `"0"` uses live price):
  ```json
  {
    "segment": "DERIVATIVE",
    "exchange": "NSE",
    "securityID": "40131",
    "txnType": "BUY",
    "quantity": "75",
    "price": "10",
    "product": "MARGIN"
  }
  ```
- **Response `200`**:
  ```json
  {
    "status": "success",
    "data": {
      "total_margin": 750,
      "span_margin": 0,
      "ha": false,
      "hedge_benefit": 0,
      "exposure_margin": 0,
      "available_balance": 0,
      "var_margin": 0,
      "insufficient_balance": 0,
      "delivery_margin": 0,
      "brokerage": 0,
      "charges": {
        "stt": 0,
        "exchange_charges": 0,
        "stamp_duty": 0,
        "sebi_turn_over_charges": 0,
        "brokerage": 10,
        "gst": 1.8,
        "IPFTCharges": 0,
        "total_charges": 11.8
      }
    }
  }
  ```
  - For `COMMODITY`, `charges` is `null`; service failure returns HTTP `512` `"Unable to fetch margin details"`.
  - *Postman Discrepancy*: Postman's saved `200` example omits `ha` and shows `brokerage: 20` (`gst: 3.6`, `total_charges: 23.6`).

#### 4. Order Updates WebSocket (`wss://ws-order-updates.indstocks.com/api/v1/ws/trades`)

- **Handshake Header**: `Authorization: <INDSTOCKS_ACCESS_TOKEN>` (raw token, no `Bearer` prefix).
- **Subscribe Frame**:
  ```json
  { "action": "subscribe", "mode": "order_update" }
  ```
- **Server Frame**: **Double-encoded JSON string** (decode outer JSON string, then parse JSON object):
  ```json
  {
    "mode": "order_update",
    "timestamp": 1789628366550,
    "data": {
      "order_id": "97603202",
      "entity_name": "SENSEX 17 SEP 74400 CE",
      "lot": 1,
      "order_type": "BUY",
      "order_status": "S",
      "executed_price": 306.45,
      "elapsed_time": 8,
      "error_message": " ",
      "timestamp": 1789628366544,
      "req_quantity": 20,
      "requested_lot": 1
    }
  }
  ```
- **Short `order_status` Codes**:
  - `R` — Received
  - `P` — Pending
  - `S` — Success (executed)
  - `F` — Failed
  - `C` — Cancelled
  - `RJ` — Rejected
  - `PF` — Partially filled
  - `PFC` — Partially filled, remaining cancelled
- **Notes**: Filter on `mode == "order_update"` (ignore heartbeats); sequence events by outer envelope `timestamp`; note `data.order_id` is numeric without `DRV-`/`EQ-` prefix in `Websockets/` (`"97603202"`), and `" "` (single space) in `error_message` means no error. *(Postman's markdown description shows an older illustrative payload with `"type": "order"` and `"PARTIALLY_EXECUTED"` — `Websockets/` has the live double-encoded schema.)*

---

## Part IV — Post-Trial Live-Trading Readiness Checklist

Before ever enabling live order execution after the 2-week paper-trading trial:

1. **Explicit Safety Guardrail**: Currently `LIVE_ORDER_PLACEMENT_IMPLEMENTED = false` in `src/lib.rs`, enforced in `src/main.rs`, `src/paper_runner.rs`, `tests/safety.rs`, and `tests/e2e_trading_agents.rs`. Any transition to live trading requires a dedicated, audited architectural change and live integration tests.
2. **Static IP Whitelisting (`NSE/INVG/67858`)**: Configure Primary (and optional Secondary) IPv4/IPv6 static IP(s) at `https://www.indstocks.com/app/api-trading/access-tokens`. Remember the 1-change-per-calendar-week limit; read-only paper testing does **not** require a static IP.
3. **Client-Side Trailing Stop-Loss**: Because `POST /smart/order` silently ignores `is_tsl` and `tsl_step_size`, continue running client-side trailing stop-loss monitoring and update `GTT-` legs via `POST /smart/order/modify` or exit via opposing `POST /order`.
4. **Contract Metadata & Freeze Quantities**: Look up `LOT_UNITS`, `TICK_SIZE`, and `FREEZE_QTY` from `GET /market/instruments?source=fno` or `GET /market/instruments/search`; pass `qty` in **units** (never lots) and use `slice_order: true` (<=10 slices) if exceeding `FREEZE_QTY`.
5. **Non-Standard HTTP Client Requirements**: Verify `reqwest` / HTTP client handling for `GET` requests carrying JSON bodies (`GET /order`, `GET /order/trades`, `GET /margin`) and handle HTTP `512` (`InternalServerException`) on `GET /order` as terminal (non-retryable).
6. **Live Smoke Test of Unverified Gaps**: With a live `INDSTOCKS_ACCESS_TOKEN`, smoke-test:
   - Whether `NIDX_40000001` works on `/market/quotes/*` (or keep `/market/option-chain` `underlying_ltp`),
   - Whether `market_depth` returns flat or scrip-nested JSON in live `/market/quotes/full` & `/mkt`,
   - Which token (`40000001` vs `NIDX:26000`) `ws-prices.indstocks.com` expects for NIFTY 50, and whether `NFO:` supports `quote` mode,
   - Whether `POST /generate/token` (currently provisional) is active on your account if automated refresh is later desired.
7. **Reconciliation & Risk Audit**: Reconcile `wss://ws-order-updates.indstocks.com/api/v1/ws/trades` (double-encoded JSON) with `GET /order-book` and `GET /portfolio/positions`, verify `GET /funds` & `GET /margin` before order submission, and audit `settings.toml` `[paper]` limits for real capital sizing.
