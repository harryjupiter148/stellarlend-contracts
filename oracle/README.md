# StellarLend Oracle Service

Off-chain oracle integration service that fetches price data from multiple external sources and updates the smart contract on Soroban.

## Features

- **Multi-Source Price Fetching**: Aggregates prices from CoinGecko and Binance
- **Price Validation**: Validates prices for staleness, deviation, and bounds
- **Weighted Median**: Calculates weighted median from multiple sources for accuracy
- **Efficient Caching**: In-memory caching with configurable TTL to reduce API calls

## Prerequisites

- Node.js >= 18.0.0
- npm

## Installation

```bash
cd oracle
npm install
```

## Configuration

Copy the example environment file and configure:

```bash
cp .env.example .env
```

### Environment Variables

| Variable | Description | Required |
|----------|-------------|----------|
| `STELLAR_NETWORK` | Network: `testnet` or `mainnet` | Yes |
| `STELLAR_RPC_URL` | Soroban RPC endpoint | Yes |
| `CONTRACT_ID` | StellarLend contract address | Yes |
| `ADMIN_SECRET_KEY` | Secret key for signing transactions | Yes |
| `COINGECKO_API_KEY` | CoinGecko Pro API key | No |
| `CACHE_TTL_SECONDS` | Cache TTL in seconds (default: 30) | No |
| `UPDATE_INTERVAL_MS` | Price update interval (default: 60000) | No |
| `MAX_PRICE_DEVIATION_PERCENT` | Max price deviation % (default: 10) | No |
| `PRICE_BOUNDS_JSON` | Optional JSON map of per-asset min/max bounds | No |
| `ADMIN_API_PORT` | HTTP port for secure admin operations | No |
| `ADMIN_HMAC_SECRET` | HMAC secret used to sign admin reload requests | No |
| `LOG_LEVEL` | Logging: debug, info, warn, error | No |

## Usage

### Development

```bash
npm run dev
```

### Production

```bash
npm run build
npm start
```

### Admin reload endpoint

The oracle service can expose a secure admin endpoint when `ADMIN_API_PORT` is configured.
Requests to `POST /reload-config` must include an `x-signature` header containing a hex HMAC-SHA256 over the raw request body using `ADMIN_HMAC_SECRET`.
The payload may include `validatorConfig` updates and/or asset-specific `bounds` to tighten min/max price ranges.

Example payload:

```json
{
  "bounds": {
    "XLM": { "minPrice": 0.1, "maxPrice": 1000000 }
  }
}
```

This endpoint is intended for emergency tightening of bounds without restarting the service.

### Testing

```bash
npm test                 # Run all tests
npm run test:coverage    # With coverage report
npm run test:watch       # Watch mode
```

## Live Integration Test

To verify proper operation with real APIs (CoinGecko, Binance), run the live test script:

```bash
npx tsx tests/live-test.ts
```

This script will:
1. Initialize the CoinGecko and Binance providers.
2. Fetch live prices for XLM and BTC from each.
3. Aggregate the prices and display the result.

## Supported Assets

| Asset | CoinGecko | Binance |
|-------|-----------|---------|
| XLM   | Yes       | Yes     |
| USDC  | Yes       | Yes     |
| BTC   | Yes       | Yes     |
| ETH   | Yes       | Yes     |
| SOL   | Yes       | Yes     |

## Price Sources

### CoinGecko (Primary)
- Popular crypto price API
- Priority: 1, Weight: 60%

### Binance (Secondary)
- Public market data API
- Priority: 2, Weight: 40%

## Programmatic Usage

```typescript
import { OracleService, loadConfig } from 'stellarlend-oracle';

const config = loadConfig();
const service = new OracleService(config);

// Start automatic updates
await service.start(['XLM', 'USDC', 'BTC']);

// Or fetch manually
const price = await service.fetchPrice('XLM');

// Stop service
service.stop();
```

## Project Structure

```
oracle/
├── src/
│   ├── index.ts              # Main entry point
│   ├── config.ts             # Configuration
│   ├── providers/            # Price providers
│   │   ├── coingecko.ts      # CoinGecko API
│   │   └── binance.ts        # Binance API
│   ├── services/             # Core services
│   │   ├── price-validator.ts
│   │   ├── price-aggregator.ts
│   │   ├── cache.ts
│   │   └── contract-updater.ts
│   ├── types/                # TypeScript types
│   └── utils/                # Utilities
├── tests/                    # Test suites
└── package.json
```

## Cheers!
