# Ledger Export Consumer

`examples/plugin-ledger-consumer.py` is a tiny read-only consumer for
`truss ledger export`. It demonstrates the safest plugin class: observe a signed
evidence bundle, verify the status, and render a derived view without touching
the runtime database.

```bash
truss --db harness.db ledger export <run-id> \
  | python3 examples/plugin-ledger-consumer.py --fail-on-unverified
```

```python
{{#include ../../examples/plugin-ledger-consumer.py}}
```
