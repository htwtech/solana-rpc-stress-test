# Определите переменные
RPC_URL="https://api.mainnet-beta.solana.com"
TX_HASH="4ydFqGpffYTwdCwSaB9sYP6AvUhsUSexGxhZCxytJkiPnHCKz8PSvcGcjJ9SEi6d6XHbNMiDH6u5wFbaK1LZejoA"

curl $RPC_URL -X POST -H "Content-Type: application/json" -d '
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "getTransaction",
  "params": [
    "'$TX_HASH'",
    {
      "encoding": "base64",
      "maxSupportedTransactionVersion": 0
    }
  ]
}'
