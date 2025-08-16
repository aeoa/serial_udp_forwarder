//! A simple command‑line application that forwards delimited messages from a
//! serial port to a UDP endpoint.
//!
//! This binary provides two subcommands: `list-devices` prints out the
//! available serial ports along with any USB vendor/product information,
//! while `run` connects to a specified device, reads lines terminated by a
//! configurable byte sequence and forwards them to a UDP socket.  The
//! forwarding is performed asynchronously; a reader task pushes complete
//! messages into a bounded channel and a writer task drains that channel and
//! sends the messages over UDP.  A statistics task periodically prints
//! throughput information to the console.

use std::borrow::Cow;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, Args};
use tokio::io::{AsyncReadExt};
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, Notify};
use std::collections::VecDeque;
use tokio::time::{self, Duration};

use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_serial::SerialPortType;

/// Command line interface for the serial‑UDP forwarder.
#[derive(Parser, Debug)]
#[command(name = "serial_udp_forwarder", author, version, about = "Forward serial port messages to UDP")] 
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Supported subcommands.
#[derive(Subcommand, Debug)]
enum Commands {
    /// List all detected serial devices along with USB metadata.
    ListDevices,
    /// Run the serial to UDP forwarder.
    Run(RunArgs),
}

/// Arguments specific to the `run` subcommand.
#[derive(Args, Debug)]
struct RunArgs {
    /// Path/name of the serial port (e.g. `/dev/ttyUSB0` or `COM3`).
    /// If omitted, a device is selected based on the vendor/product identifiers
    /// or name substring.
    #[arg(long)]
    port: Option<String>,

    /// USB vendor ID to select the port by (accepts decimal or hex, e.g. `0x0403`)
    #[arg(long)]
    vid: Option<String>,

    /// USB product ID to select the port by (accepts decimal or hex)
    #[arg(long)]
    pid: Option<String>,

    /// Substring to match against the device manufacturer string (USB only).
    /// Matching is case insensitive.
    #[arg(long)]
    manufacturer: Option<String>,

    /// Substring to match against the device product string (USB only).
    /// Matching is case insensitive.
    #[arg(long)]
    product: Option<String>,

    /// Serial number or substring to match against the device's USB serial number.
    /// Matching is case insensitive.  This can be used instead of specifying
    /// a port path or VID/PID to uniquely identify a device by its built‑in
    /// serial number.
    #[arg(long)]
    serial: Option<String>,

    /// Baud rate for the serial port
    #[arg(long, default_value_t = 9_600)]
    baud: u32,

    /// Delimiter that terminates a message.  Escape sequences such as `\n`,
    /// `\r` and `\t` are supported.
    #[arg(long, default_value = "\n")]
    terminator: String,

    /// Destination IP address for UDP
    #[arg(long, default_value = "127.0.0.1")]
    udp_ip: String,

    /// Destination UDP port
    #[arg(long)]
    udp_port: u16,

    /// Maximum number of in‑flight messages.  When the buffer is full, the
    /// oldest message will be dropped.
    #[arg(long, default_value_t = 100)]
    buffer: usize,

    /// Interval in seconds at which throughput statistics are printed
    #[arg(long, default_value_t = 1)]
    stats_interval: u64,

    /// Show the last received message alongside the throughput statistics.
    /// When set, the most recent complete message read from the serial port
    /// will be printed together with the messages‑per‑second report.  This
    /// helps monitor incoming data without flooding the console with every
    /// line.
    #[arg(long, default_value_t = false)]
    show_last: bool,

}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::ListDevices => {
            list_devices().await
        }
        Commands::Run(args) => {
            run_forwarder(args).await
        }
    }
}

/// List available serial devices along with any USB vendor/product information.
async fn list_devices() -> Result<()> {
    // Use tokio‑serial's enumeration to obtain `SerialPortInfo` structures.  On
    // systems where libudev is available this will include detailed USB
    // metadata such as the vendor and product identifiers【41378760481648†L41-L49】.
    let ports = tokio_serial::available_ports()
        .context("failed to enumerate serial ports")?;
    if ports.is_empty() {
        println!("No serial ports detected.");
        return Ok(());
    }
    println!("Detected serial ports:");
    for info in ports {
        print!("{}", info.port_name);
        match info.port_type {
            SerialPortType::UsbPort(ref usb) => {
                print!(" (USB vid: 0x{:04x}, pid: 0x{:04x}", usb.vid, usb.pid);
                if let Some(ref manuf) = usb.manufacturer {
                    print!(", manufacturer: {}", manuf);
                }
                if let Some(ref prod) = usb.product {
                    print!(", product: {}", prod);
                }
                if let Some(ref sn) = usb.serial_number {
                    print!(", serial: {}", sn);
                }
                print!(")");
            }
            SerialPortType::BluetoothPort => {
                print!(" (Bluetooth)");
            }
            SerialPortType::PciPort => {
                print!(" (PCI)");
            }
            SerialPortType::Unknown => {
                print!(" (Unknown type)");
            }
        }
        println!();
    }
    Ok(())
}

/// Attempt to parse a string as either a decimal or hexadecimal `u16`.
fn parse_u16_flex(value: &str) -> Option<u16> {
    // Try hexadecimal prefix 0x or 0X
    if let Some(stripped) = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        return u16::from_str_radix(stripped, 16).ok();
    }
    // Try to parse as hex if it contains any hex letters
    if value.chars().any(|c| c.is_ascii_hexdigit() && c.is_ascii_alphabetic()) && value.chars().all(|c| c.is_ascii_hexdigit()) {
        return u16::from_str_radix(value, 16).ok();
    }
    // Fallback to decimal
    value.parse::<u16>().ok()
}

/// Decode a terminator string into a sequence of bytes.  Supports common
/// escape sequences such as `\n` (newline), `\r` (carriage return), `\t`
/// (tab) and `\\` (backslash).  Any unknown escape sequence is treated
/// literally.
fn decode_terminator(input: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                match next {
                    'n' => bytes.push(b'\n'),
                    'r' => bytes.push(b'\r'),
                    't' => bytes.push(b'\t'),
                    '0' => bytes.push(b'\0'),
                    '\\' => bytes.push(b'\\'),
                    _ => {
                        // Unknown escape – treat both characters literally
                        bytes.push(next as u8);
                    }
                }
            } else {
                // Trailing backslash
                bytes.push(b'\\');
            }
        } else {
            bytes.push(c as u8);
        }
    }
    bytes
}

/// Find the serial port that matches the provided selectors.  The first
/// non‑none criteria among `vid`/`pid` or `name` is used.  If a port name
/// is provided directly it is returned without enumeration.
fn select_port(args: &RunArgs) -> Result<String> {
    if let Some(ref port) = args.port {
        return Ok(port.clone());
    }
    let ports = tokio_serial::available_ports()
        .context("failed to enumerate serial ports")?;
    if ports.is_empty() {
        return Err(anyhow!("no serial ports detected"));
    }
    // Pre‑process selectors
    let vid = args.vid.as_ref().and_then(|s| parse_u16_flex(s));
    let pid = args.pid.as_ref().and_then(|s| parse_u16_flex(s));
    let manufacturer_substr = args.manufacturer.as_ref().map(|s| s.to_lowercase());
    let product_substr = args.product.as_ref().map(|s| s.to_lowercase());
    let serial_substr = args.serial.as_ref().map(|s| s.to_lowercase());
    // Collect all ports that satisfy all provided selection criteria.  When
    // multiple identifiers are supplied they are applied conjunctively.  If
    // multiple ports match the criteria we report an error rather than
    // arbitrarily picking the first one.
    let mut matches: Vec<String> = Vec::new();
    for info in ports {
        // Apply VID/PID filter if specified.  When a vendor ID is provided we
        // only consider USB ports.  The product ID must match if provided.
        if let Some(v) = vid {
            match info.port_type {
                SerialPortType::UsbPort(ref usb) => {
                    if usb.vid != v {
                        continue;
                    }
                    if let Some(p) = pid {
                        if usb.pid != p {
                            continue;
                        }
                    }
                }
                _ => {
                    continue;
                }
            }
        }
        // Apply manufacturer substring filter if specified.  Only USB ports
        // have manufacturer strings.  If the manufacturer does not contain
        // the substring, skip the port.
        if let Some(ref substr) = manufacturer_substr {
            match info.port_type {
                SerialPortType::UsbPort(ref usb) => {
                    match usb.manufacturer.as_ref() {
                        Some(man) => {
                            if !man.to_lowercase().contains(substr) {
                                continue;
                            }
                        }
                        None => {
                            continue;
                        }
                    }
                }
                _ => {
                    // Non‑USB ports do not have a manufacturer string
                    continue;
                }
            }
        }

        // Apply product substring filter if specified.  Only USB ports
        // have product strings.  If the product does not contain the
        // substring, skip the port.
        if let Some(ref substr) = product_substr {
            match info.port_type {
                SerialPortType::UsbPort(ref usb) => {
                    match usb.product.as_ref() {
                        Some(prod) => {
                            if !prod.to_lowercase().contains(substr) {
                                continue;
                            }
                        }
                        None => {
                            continue;
                        }
                    }
                }
                _ => {
                    // Non‑USB ports do not have a product string
                    continue;
                }
            }
        }
        // Apply serial number substring filter if specified.  Only USB ports
        // have serial numbers.  If the port is not USB or the serial
        // number does not contain the substring, skip the port.
        if let Some(ref substr) = serial_substr {
            match info.port_type {
                SerialPortType::UsbPort(ref usb) => {
                    match usb.serial_number.as_ref() {
                        Some(sn) => {
                            if !sn.to_lowercase().contains(substr) {
                                continue;
                            }
                        }
                        None => {
                            continue;
                        }
                    }
                }
                _ => {
                    continue;
                }
            }
        }
        // If we reach here the port satisfies all specified filters
        matches.push(info.port_name);
    }
    match matches.len() {
        0 => Err(anyhow!("no serial port matched the given selectors")),
        1 => Ok(matches.remove(0)),
        _ => {
            // On macOS each physical port appears twice (cu.* and tty.*).  If the
            // only difference between the matches is the cu/tty prefix we treat
            // them as duplicates and select the first one.  We collapse
            // duplicates by stripping the common prefix and grouping by the
            // remainder.  If multiple unique groups remain after deduplication
            // then we still consider it ambiguous.
            fn unify_name(name: &str) -> &str {
                // Normalize cu/tty device names by removing the prefix.  The
                // prefixes are "/dev/cu." (8 bytes) and "/dev/tty." (9 bytes).
                if name.starts_with("/dev/cu.") {
                    &name[8..]
                } else if name.starts_with("/dev/tty.") {
                    &name[9..]
                } else {
                    name
                }
            }
            use std::collections::HashMap;
            let mut map: HashMap<String, String> = HashMap::new();
            for port in &matches {
                let key = unify_name(port).to_string();
                map.entry(key).or_insert_with(|| port.clone());
            }
            match map.len() {
                0 => Err(anyhow!("no serial port matched the given selectors")),
                1 => {
                    // All matches map to the same normalized name: pick the first
                    Ok(map.into_values().next().unwrap())
                }
                _ => {
                    // More than one unique normalized name remains, still ambiguous
                    Err(anyhow!("multiple serial ports match the given selectors: {}", matches.join(", ")))
                }
            }
        }
    }
}

/// Spawn an asynchronous reader that fills a bounded ring buffer with complete
/// messages.  When the buffer is full the oldest message is dropped to make
/// room for the new one.  A `Notify` is used to signal the writer task that
/// data is available.  Optionally updates the last message tracker for the
/// statistics printer.
fn spawn_serial_reader_ring(
    mut serial: SerialStream,
    delimiter: Vec<u8>,
    queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    capacity: usize,
    notify: Arc<Notify>,
    last_msg: Option<Arc<Mutex<Option<Vec<u8>>>>>,
) {
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let mut temp = [0u8; 512];
        loop {
            match serial.read(&mut temp).await {
                Ok(n) if n > 0 => {
                    for &byte in &temp[..n] {
                        buffer.push(byte);
                        if buffer.ends_with(&delimiter) {
                            let msg_len = buffer.len() - delimiter.len();
                            let msg = buffer[..msg_len].to_vec();
                            // Update last message tracker if provided
                            if let Some(ref last) = last_msg {
                                let mut guard = last.lock().await;
                                *guard = Some(msg.clone());
                            }
                            // Push the message into the ring buffer, dropping the
                            // oldest entry if the capacity is reached.
                            {
                                let mut q = queue.lock().await;
                                if q.len() >= capacity {
                                    q.pop_front();
                                }
                                q.push_back(msg);
                            }
                            // Notify the writer that a message is available
                            notify.notify_one();
                            buffer.clear();
                        }
                    }
                }
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("Error reading from serial port: {e}");
                    return;
                }
            }
        }
    });
}

/// Spawn an asynchronous writer that drains messages from the bounded ring
/// buffer and sends them over UDP.  It waits for notifications from the
/// reader and processes all queued messages before waiting again.  Each
/// successful send increments the provided counter.
fn spawn_udp_writer_ring(
    socket: UdpSocket,
    queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    notify: Arc<Notify>,
    counter: Arc<AtomicU64>,
    remote_addr: String,
) {
    tokio::spawn(async move {
        loop {
            // Wait until notified that at least one message is available
            notify.notified().await;
            loop {
                // Pop a message from the queue, if any
                let msg_opt = {
                    let mut q = queue.lock().await;
                    q.pop_front()
                };
                match msg_opt {
                    Some(msg) => {
                        match socket.send_to(&msg, &remote_addr).await {
                            Ok(_) => {
                                counter.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                // On some systems a send_to to a port with no listener
                                // will return a connection refused error.  We
                                // silently ignore this specific error to avoid
                                // spamming the console.  Other errors are still
                                // printed.
                                if e.kind() != std::io::ErrorKind::ConnectionRefused {
                                    eprintln!("Error sending UDP packet: {e}");
                                }
                            }
                        }
                    }
                    None => {
                        break;
                    }
                }
            }
        }
    });
}

/// Spawn a periodic statistics task that prints messages per interval to stdout.
fn spawn_stats_printer(
    counter: Arc<AtomicU64>,
    interval_secs: u64,
    last_msg: Option<Arc<Mutex<Option<Vec<u8>>>>>,
) {
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(interval_secs));
        let mut last_count = 0u64;
        loop {
            interval.tick().await;
            let total = counter.load(Ordering::Relaxed);
            let delta = total - last_count;
            // Compose the base statistics string
            let mut msg = format!(
                "Forwarded {} messages in the last {}s (total: {})",
                delta, interval_secs, total
            );
            // If a last message tracker is provided, include it
            if let Some(ref last) = last_msg {
                if let Ok(guard) = last.try_lock() {
                    if let Some(ref data) = *guard {
                        // Convert the last message bytes to a printable string.  Use
                        // lossless UTF‑8 conversion when possible, otherwise show
                        // hex representation to avoid invalid UTF‑8 output.
                        let preview = match std::str::from_utf8(data) {
                            Ok(s) => s.trim_end_matches(|c: char| c.is_control()).to_string(),
                            Err(_) => {
                                // Represent as hex (truncate if necessary)
                                let mut hex = String::new();
                                for (i, byte) in data.iter().take(32).enumerate() {
                                    if i > 0 {
                                        hex.push(' ');
                                    }
                                    hex.push_str(&format!("{:02x}", byte));
                                }
                                if data.len() > 32 {
                                    hex.push_str(" …");
                                }
                                hex
                            }
                        };
                        msg.push_str(&format!(" | last: {}", preview));
                    }
                }
            }
            println!("{}", msg);
            last_count = total;
        }
    });
}

/// Execute the `run` subcommand.  This function resolves the port name,
/// opens the serial port asynchronously, sets up the UDP socket, and
/// orchestrates the concurrent reader and writer tasks.  It waits for a
/// Ctrl‑C signal to gracefully shut down.
async fn run_forwarder(args: RunArgs) -> Result<()> {
    // Resolve the port to connect to
    let port_name = select_port(&args)?;
    println!("Using serial port: {}", port_name);

    // Build and open the serial port asynchronously.  The
    // `open_native_async` method is provided by the `SerialPortBuilderExt`
    // trait【344358493076465†L120-L150】 and returns a `SerialStream` which
    // implements both `AsyncRead` and `AsyncWrite`【390261871706494†L303-L339】.
    let builder = tokio_serial::new(Cow::from(port_name.as_str()), args.baud);
    let serial = builder
        .open_native_async()
        .with_context(|| format!("failed to open serial port {port_name}"))?;

    // Prepare UDP socket.  We do not connect the socket to avoid
    // connected‑UDP semantics (ICMP Port Unreachable errors).  The remote
    // address is passed to the writer task and used with send_to().
    let remote_addr = format!("{}:{}", args.udp_ip, args.udp_port);
    let socket = UdpSocket::bind("0.0.0.0:0").await
        .context("failed to bind UDP socket")?;

    // Bounded ring buffer for complete messages.  When the capacity is
    // exceeded the oldest entry is dropped.  Access to the buffer is
    // synchronized via a mutex, and a notify is used to signal the writer.
    let queue: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let notify = Arc::new(Notify::new());
    let counter = Arc::new(AtomicU64::new(0));

    // Decode the terminator string into bytes
    let delimiter = decode_terminator(&args.terminator);

    // Prepare optional last message tracker if the user wants to display it.
    let last_msg: Option<Arc<Mutex<Option<Vec<u8>>>>> = if args.show_last {
        Some(Arc::new(Mutex::new(None)))
    } else {
        None
    };

    // Spawn tasks using the ring buffer.  The reader pushes messages into
    // the queue and notifies the writer.  The writer drains the queue and
    // sends the messages over UDP.  Both tasks operate asynchronously.
    spawn_serial_reader_ring(
        serial,
        delimiter,
        Arc::clone(&queue),
        args.buffer,
        Arc::clone(&notify),
        last_msg.clone(),
    );
    spawn_udp_writer_ring(
        socket,
        Arc::clone(&queue),
        Arc::clone(&notify),
        Arc::clone(&counter),
        remote_addr.clone(),
    );
    spawn_stats_printer(Arc::clone(&counter), args.stats_interval, last_msg);

    println!("Started forwarding. Press Ctrl+C to stop.");

    // Wait for Ctrl‑C (SIGINT) to shut down.  Without this the program would
    // exit immediately after spawning the tasks.
    tokio::signal::ctrl_c().await?;
    println!("Received interrupt, shutting down...");
    Ok(())
}