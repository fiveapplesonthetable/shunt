# Notification delivery

`pip install ./plugins/notify` installs a standard-library delivery journal for
project notification hooks. The project supplies its transport and notification
key; the plugin records the claim before invoking the transport and its receipt
after acceptance. A sent key is acknowledged without invoking the transport again.

```python
from shunt_notify import deliver_once

status = deliver_once(receipts_directory, notification_key, send_message)
```

The callback returns after confirmed acceptance. It raises `NotSent` only when
it can establish that the transport did not accept the message. Other exceptions
leave an uncertain outcome. A process lost after claiming a delivery also leaves
an uncertain outcome; it is never resent automatically. Concurrent callers see
an active claim, using the process start identity where the host provides one.

`inspect_delivery` returns the receipt. `resolve_uncertain(..., sent=True)` records
confirmed acceptance; `sent=False` permits a later delivery attempt. Resolution
does not invoke the transport and refuses a still-active delivery. The
`shunt-notify --home DIR status KEY` and `resolve KEY --sent|--not-sent` commands
expose the same operations. The journal stores keys and delivery state, not message
bodies or transport credentials.
