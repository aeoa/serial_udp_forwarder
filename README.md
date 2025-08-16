# serial_udp_forwarder
Forward serial port messages to UDP
```
Usage: serial_udp_forwarder <COMMAND>

Commands:
  list-devices  List all detected serial devices along with USB metadata
  run           Run the serial to UDP forwarder
  help          Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

## list-devices
List all detected serial devices along with USB metadata
```
Usage: serial_udp_forwarder list-devices

Options:
  -h, --help  Print help
```
## run
Run the serial to UDP forwarder
```
Usage: serial_udp_forwarder run [OPTIONS] --udp-port <UDP_PORT>

Options:
      --port <PORT>
          Path/name of the serial port (e.g. `/dev/ttyUSB0` or `COM3`). If omitted, a device is selected based on the vendor/product identifiers or name substring
      --vid <VID>
          USB vendor ID to select the port by (accepts decimal or hex, e.g. `0x0403`)
      --pid <PID>
          USB product ID to select the port by (accepts decimal or hex)
      --manufacturer <MANUFACTURER>
          Substring to match against the device manufacturer string (USB only). Matching is case insensitive
      --product <PRODUCT>
          Substring to match against the device product string (USB only). Matching is case insensitive
      --serial <SERIAL>
          Serial number or substring to match against the device's USB serial number. Matching is case insensitive.  This can be used instead of specifying a port path or VID/PID to uniquely identify a device by its built‑in serial number
      --baud <BAUD>
          Baud rate for the serial port [default: 9600]
      --terminator <TERMINATOR>
          Delimiter that terminates a message.  Escape sequences such as `\n`, `\r` and `\t` are supported [default: "\n"]
      --udp-ip <UDP_IP>
          Destination IP address for UDP [default: 127.0.0.1]
      --udp-port <UDP_PORT>
          Destination UDP port
      --buffer <BUFFER>
          Maximum number of in‑flight messages.  When the buffer is full, reads from the serial port will pause until the sender drains some messages [default: 100]
      --stats-interval <STATS_INTERVAL>
          Interval in seconds at which throughput statistics are printed [default: 1]
      --show-last
          Show the last received message alongside the throughput statistics. When set, the most recent complete message read from the serial port will be printed together with the messages‑per‑second report.  This helps monitor incoming data without flooding the console with every line
  -h, --help
          Print help
```
