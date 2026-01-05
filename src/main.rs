// By: Eric MacDonald (eMac)

use clap::Parser;
use log;
use clap_verbosity_flag::Verbosity;
use std::str;
use std::io::ErrorKind::{ConnectionReset, ConnectionAborted};
use futures::FutureExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::broadcast;
use tokio::process::Command;
use std::process::Stdio;
use std::{thread, time};

const BUFFER_SIZE: usize = 0x1000;
const DEFAULT_IP: &'static str = "127.0.0.1";
const WINCE_COMMAND_DELAY_MS: u64 = 1000;

// The line feed and carriage return chars are used to mark the end of the command string and to begin parsing.
const CCHAR_LINE_FEED: u8 = 0x0a;
const CCHAR_CARRIAGE_RETURN: u8 = 0x0d;

/// WebTV TouchPPP
///
/// Provides a way for the WebTV MAME driver to talk with PPP using its null modem.
#[derive(Parser, Debug)]
#[command(
    version,
    about,
    long_about
)]
#[clap(
    version = "1.5",
    author = "wtvemac",
    after_help = "Special thanks to: Zefie, MattMan, and others in the WebTV hacking community!"
)]
struct CmdOpts {
    /// The socket address to listen on. 127.0.0.1 is used as the IP if just the port is given.
    ///
    /// Example: -l 6400
    #[arg(
        short,
        long,
        value_name = "[HOST:]PORT",
        default_value_t = format!("{}:{}", DEFAULT_IP, 1122)
    )]
    listen: String,
    /// The remote server that provides PPP communication. Either this or the -e option can be used.
    ///
    /// Example: -c ppp.cool.com:2323
    #[arg(
        short,
        long,
        value_name = "HOST:PORT",
        default_value = "127.0.0.1:2323"
    )]
    connect: Option<String>,
    /// PPP command to run for direct PPP communication. Either this or the -c option can be used.
    ///
    /// Example: -e '/usr/sbin/pppd notty'
    #[arg(
        short,
        long,
        value_name="/path/to/exe exe_options"
    )]
    exec: Option<String>,

    // -q = silent
    // default = errors
    // -v = errors and warnings
    // -vv = errors, warnings and simple info
    // -vvv = errors, warnings, simple info and debug info
    // -vvvv = errors, warnings, simple info, debug info and tracing info
    #[command(flatten)]
    verbosity: Verbosity
}

async fn copy_loop<R, W>(
    read: &mut R,
    write: &mut W,
    is_mame: bool,
    mame_socket_address: &String,
    mut abort: broadcast::Receiver<()>,
) -> tokio::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut copied_bytes = 0;
    let mut buf = [0u8; BUFFER_SIZE];
    let mut at_string: String = "".to_string();
    'conn: loop {
        let bytes_found;
        tokio::select! {
            biased;

            result = read.read(&mut buf) => {
                bytes_found = result.or_else(|e| match e.kind() {
                    ConnectionReset | ConnectionAborted => Ok(0),
                    _ => Err(e)
                })?;
            },
            _ = abort.recv() => {
                break 'conn;
            }
        }

        if bytes_found == 0 {
            break 'conn;
        }


        if is_mame {
            log::trace!("[<{mame_socket_address}] {:x?}", &buf[0..bytes_found]);

            // This is very crude but gets the job done.
            for i in 0..bytes_found {
                if buf[i] >= 0x0a && buf[i] < 0x7a {
                    let s = String::from_utf8_lossy(&buf[i..i+1]);
                    at_string.push_str(&s);

                    if (at_string.len() >= 2 && (!at_string.starts_with("AT") && !at_string.starts_with("++"))) || at_string.len() > 50 {
                        at_string = "".to_string();
                    } else if at_string.contains("+++") {
                        log::info!("[{mame_socket_address}] Client requesting command mode with +++. Disconnecting and going back to command state.");
                        break 'conn;
                    } else if at_string.len() >= 5 && (buf[i] == CCHAR_LINE_FEED || buf[i] == CCHAR_CARRIAGE_RETURN) {
                        if at_string.starts_with("AT") {
                            log::info!("[{mame_socket_address}] AT command in PPP traffic detected. Disconnecting and going back to command state.");
                            break 'conn;
                        }

                        at_string = "".to_string();
                    }
                } else {
                    at_string = "".to_string();
                }
            }
        } else {
            log::trace!("[>{mame_socket_address}] {:x?}", &buf[0..bytes_found]);
        }

        write.write_all(&buf[0..bytes_found]).await?;
        copied_bytes += bytes_found;
    }

    Ok(copied_bytes)
}

async fn local_exec_loop(mame: &mut TcpStream, mame_socket_address: &String, local_program_command: &String) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let (mut mame_reader, mut mame_writer) = mame.split();

    let mut the_args = local_program_command.split(' '); 
    let first: &str = the_args.next().unwrap();
    let rest: Vec<&str> = the_args.collect::<Vec<&str>>();

    log::debug!("Got it? '{}'", first);
    log::debug!("Got it2? '{}'", local_program_command);

    let mut ppp = match Command::new(first)
        .args(rest)
        .stdout(Stdio::piped())
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn() {
        Ok(r) => r,
        Err(e) => {
            log::error!("Unable to launch PPP! {e}");

            return Ok((0, 0));
        },
    };

    let mut ppp_reader = BufReader::new(ppp.stdout.take().expect("No PPP STDOUT?"));
    let mut ppp_writer = BufWriter::new(ppp.stdin.take().expect("No PPP STDIN?"));

    let (cancel, _) = broadcast::channel::<()>(1);

    let (ppp_to_mame_copied_bytes, mame_to_ppp_copied_bytes) = tokio::join!{
        copy_loop(&mut ppp_reader, &mut mame_writer, false, mame_socket_address, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
        copy_loop(&mut mame_reader, &mut ppp_writer, true, mame_socket_address, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
    };

    Ok((mame_to_ppp_copied_bytes.unwrap(), ppp_to_mame_copied_bytes.unwrap()))
}

async fn remote_ppp_loop(mame: &mut TcpStream, mame_socket_address: &String, ppp_socket_address: &String) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let mut ppp: TcpStream = match TcpStream::connect(ppp_socket_address).await {
        Ok(r) => r,
        Err(e) => {
            log::error!("Couldn't touch PPP: error={e}");

            return Ok((0, 0));
        }
    };

    let (mut mame_reader, mut mame_writer) = mame.split();
    let (mut ppp_reader, mut ppp_writer) = ppp.split();

    let (cancel, _) = broadcast::channel::<()>(1);

    let (ppp_to_mame_copied_bytes, mame_to_ppp_copied_bytes) = tokio::join!{
        copy_loop(&mut ppp_reader, &mut mame_writer, false, mame_socket_address, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
        copy_loop(&mut mame_reader, &mut ppp_writer, true, mame_socket_address, cancel.subscribe())
            .then(|r| { let _ = cancel.send(()); async { r } }),
    };

    Ok((mame_to_ppp_copied_bytes.unwrap(), ppp_to_mame_copied_bytes.unwrap()))
}

async fn start_ppp_loop(mame: &mut TcpStream, local_program_command: &String, ppp_socket_address: &String) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let mame_to_ppp_copied_bytes ;
    let ppp_to_mame_copied_bytes ;

    if local_program_command != "" {
        log::info!("[{:?}] Launching then touching some PPP! '{}'", mame.peer_addr()?, local_program_command);

        (mame_to_ppp_copied_bytes, ppp_to_mame_copied_bytes) = match local_exec_loop(mame, &mame.peer_addr()?.to_string(), local_program_command).await {
            Ok(r) => r,
            Err(e) => {
                return Err(e);
            }
        };
    } else {
        log::info!("[{:?}] Touching PPP! '{}'", mame.peer_addr()?, ppp_socket_address);

        (mame_to_ppp_copied_bytes, ppp_to_mame_copied_bytes) = match remote_ppp_loop(mame, &mame.peer_addr()?.to_string(), ppp_socket_address).await {
            Ok(r) => r,
            Err(e) => {
                return Err(e);
            }
        };
    }

    log::info!("[{:?}] Looks like the MAME is done? Taking my hands off PPP. {mame_to_ppp_copied_bytes} bytes copied from MAME to PPP; {ppp_to_mame_copied_bytes} bytes copied from PPP to MAME", mame.peer_addr()?);

    Ok((mame_to_ppp_copied_bytes, ppp_to_mame_copied_bytes))
}

async fn send_result(mame: &mut TcpStream, short_code: &[u8], lookup_long_result: bool, leading_white_space: bool) -> Result<(), std::io::Error> {
    if leading_white_space {
        if let Err(e) = mame.write_all(b"\x0d\x0a").await {
            return Err(e);
        }
    }

    if lookup_long_result {
        let long_result: &[u8] = match short_code {
            b"0" => b"OK",
            b"1" => b"CONNECT",
            b"2" => b"RING",
            b"3" => b"NO CARRIER",
            b"4" => b"ERROR",
            b"5" => b"CONNECT 1200",
            b"6" => b"NO DIALTONE",
            b"7" => b"BUSY",
            b"8" => b"NO ANSWER",
            b"9" => b"CONNECT 0600",
            b"10" => b"CONNECT 2400",
            b"11" => b"CONNECT 4800",
            b"12" => b"CONNECT 9600",
            b"13" => b"CONNECT 7200",
            b"14" => b"CONNECT 12000",
            b"15" => b"CONNECT 14400",
            b"16" => b"CONNECT 19200",
            b"17" => b"CONNECT 38400",
            b"18" => b"CONNECT 57600",
            b"19" => b"CONNECT 115200",
            b"20" => b"CONNECT 230400",
            b"22" => b"CONNECT 75TX/1200RX",
            b"23" => b"CONNECT 1200TX/75RX",
            b"24" => b"DELAYED",
            b"32" => b"BLACKLISTED",
            b"33" => b"FAX",
            b"35" => b"DATA",
            b"40" => b"CARRIER 300",
            b"44" => b"CARRIER 1200/75",
            b"45" => b"CARRIER 75/1200",
            b"46" => b"CARRIER 1200",
            b"47" => b"CARRIER 2400",
            b"48" => b"CARRIER 4800",
            b"49" => b"CARRIER 7200",
            b"50" => b"CARRIER 9600",
            b"51" => b"CARRIER 12000",
            b"52" => b"CARRIER 14400",
            b"53" => b"CARRIER 16800",
            b"54" => b"CARRIER 19200",
            b"55" => b"CARRIER 21600",
            b"56" => b"CARRIER 24000",
            b"57" => b"CARRIER 26400",
            b"58" => b"CARRIER 28800",
            b"59" => b"CONNECT 16800",
            b"61" => b"CONNECT 21600",
            b"62" => b"CONNECT 24000",
            b"63" => b"CONNECT 26400",
            b"64" => b"CONNECT 28800",
            b"66" => b"COMPRESSION: CLASS 5",
            b"67" => b"COMPRESSION: V.42 bis",
            b"69" => b"COMPRESSION: NONE",
            b"70" => b"PROTOCOL: NONE",
            b"77" => b"PROTOCOL: LAPM",
            b"78" => b"CARRIER 31200",
            b"79" => b"CARRIER 33600",
            b"80" => b"PROTOCOL: ALT",
            b"81" => b"PROTOCOL: ALT-CELLULAR",
            b"84" => b"CONNECT 33600",
            b"91" => b"CONNECT 31200",
            b"150" => b"CARRIER 32000",
            b"151" => b"CARRIER 34000",
            b"152" => b"CARRIER 36000",
            b"153" => b"CARRIER 38000",
            b"154" => b"CARRIER 40000",
            b"155" => b"CARRIER 42000",
            b"156" => b"CARRIER 44000",
            b"157" => b"CARRIER 46000",
            b"158" => b"CARRIER 48000",
            b"159" => b"CARRIER 50000",
            b"160" => b"CARRIER 52000",
            b"161" => b"CARRIER 54000",
            b"162" => b"CARRIER 56000",
            b"165" => b"CONNECT 32000",
            b"166" => b"CONNECT 34000",
            b"167" => b"CONNECT 36000",
            b"168" => b"CONNECT 38000",
            b"169" => b"CONNECT 40000",
            b"170" => b"CONNECT 42000",
            b"171" => b"CONNECT 44000",
            b"172" => b"CONNECT 46000",
            b"173" => b"CONNECT 48000",
            b"174" => b"CONNECT 50000",
            b"175" => b"CONNECT 52000",
            b"176" => b"CONNECT 54000",
            b"177" => b"CONNECT 56000",
            b"+F4" => b"+FCERROR",
            b"V69420_WEBTV-K56_DLP" => b"V69420_WEBTV-K56_DLP",
            _ => b"OK"
        };

        if let Err(e) = mame.write_all(long_result).await {
            return Err(e);
        }
    } else {
        if let Err(e) = mame.write_all(short_code).await {
            return Err(e);
        }
    }

    if let Err(e) = mame.write_all(b"\x0d\x0a").await {
        return Err(e);
    }
 
    Ok(())
}

async fn send_webtvos_connection_result(mame: &mut TcpStream, is_56k_connect: bool, lookup_long_result: bool, leading_white_space: bool) -> Result<(), std::io::Error> {
    // Carrier speed doesn't really matter that much with MAME. TouchPPP doesn't throttle the connection either way.
    // But you do see a different "Connected at" message from the OS.
    if is_56k_connect {
        if let Err(e) = send_result(mame, b"162", lookup_long_result, leading_white_space).await { // CARRIER 56000
            return Err(e);
        }
    } else {
        if let Err(e) = send_result(mame, b"79", lookup_long_result, leading_white_space).await { // CARRIER 33600
            return Err(e);
        }
    }

    if let Err(e) = send_result(mame, b"67", lookup_long_result, leading_white_space).await { // COMPRESSION: V.42 bis
        return Err(e);
    }
    if let Err(e) = send_result(mame, b"19", lookup_long_result, leading_white_space).await { // CONNECT 115200
        return Err(e);
    }

    Ok(())
}

async fn send_wince_connection_result(mame: &mut TcpStream, lookup_long_result: bool, leading_white_space: bool) -> Result<(), std::io::Error> {
    thread::sleep(time::Duration::from_millis(WINCE_COMMAND_DELAY_MS));
    if let Err(e) = send_result(mame, b"2", lookup_long_result, leading_white_space).await { // RING
        return Err(e);
    }

    thread::sleep(time::Duration::from_millis(WINCE_COMMAND_DELAY_MS));
    if let Err(e) = send_result(mame, b"1", lookup_long_result, leading_white_space).await { // CONNECT
        return Err(e);
    }

    thread::sleep(time::Duration::from_millis(WINCE_COMMAND_DELAY_MS));

    Ok(())
}

//#[tokio::main(flavor = "multi_thread", worker_threads = 3)]
#[tokio::main]
async fn server_loop(start_cmd: &CmdOpts) -> Result<(), Box<dyn std::error::Error>> {

    let mut listen_socket_address = start_cmd.listen.clone();
    if !listen_socket_address.contains(":") {
        listen_socket_address = format!("{}:{}", DEFAULT_IP, listen_socket_address);
    }

    let remote_socket_address = match start_cmd.connect.clone() {
        Some(connect) => connect,
        _ => format!("{}:{}", DEFAULT_IP, 2323)
    };

    let local_program_command = match start_cmd.exec.clone() {
        Some(exec) => exec,
        _ => "".to_string()
    };

    let listener = TcpListener::bind(&listen_socket_address).await?;

    log::info!("Listening on {listen_socket_address}.");

    log::info!("You need to add '-spot:modem null_modem -bitb socket.{listen_socket_address}' for wtv1 or add '-solo:modem null_modem -bitb socket.{listen_socket_address}' for wtv2 to the MAME command line.");

    loop {
        let (mut mame, mame_socket_address) = listener.accept().await?;

        let remote_socket_address = remote_socket_address.clone();
        let local_program_command = local_program_command.clone();

        tokio::spawn(async move {

            let mut buf = [0; BUFFER_SIZE];
            let mut is_56k_modem = false;
            let mut is_56k_connect = false;
            let mut is_webtvos = true;
            let mut send_long_result = true;
            let mut echo_command = true;

            log::info!("Looks like we got a wild MAME @ {mame_socket_address}");

            let mut at_string: String = "".to_string();

            loop {
                let n: usize = match mame.read(&mut buf).await {
                    Ok(n) if n == 0 => return,
                    Ok(n) => n,
                    Err(e) => {
                        log::error!("Can't listen to MAME: error={e}");
                        return;
                    }
                };

                log::trace!("[<{mame_socket_address}] {:x?}", &buf[0..n]);

                if buf[0] >= 0x0a && buf[0] < 0x80 {
                    let s = String::from_utf8_lossy(&buf[0..n]);

                    at_string.push_str(&s);

                    if echo_command {
                        if let Err(e) = mame.write_all(&buf[0..n]).await {
                            log::error!("Error sending echoed text to MAME: error={e}");
                            return;
                        }
                    }
                }

                let command_ready = buf[n - 1] == CCHAR_LINE_FEED || buf[n - 1] == CCHAR_CARRIAGE_RETURN;
                if command_ready && at_string != "" {
                    log::debug!("[{mame_socket_address}] {}", at_string.replace("\x0d", "").replace("\x0a", ""));

                    if at_string.as_str().contains("S51=31") { // Don't know the S51 register details but seems to be used to disable 56k, Rockwell modem doesn't understand this
                        log::info!("[{mame_socket_address}] Well... they want me to disable 56k (and think I'm a softmodem)");
                        is_56k_connect = false;
                    } else if at_string.as_str().contains("+MS=11,1") { // Modulation select, 11,1 disables K56flex and V90
                        log::info!("[{mame_socket_address}] Well.. they want me to disable 56k (and think I'm a Rockwell hardmodem)");
                        is_56k_connect = false;
                    }

                    // Windows CE's Unimodem sends F0 at the start, while WebTV OS's TellyScripts does not.
                    // Only seen on LC2 WLD (Italian) boxes, the other WebTV Windows CE builds (UltimateTV) uses a softmodem.
                    if at_string.as_str().contains("F0") {
                        log::info!("[{mame_socket_address}] Found what looks like Windows CE's Unimodem init string.");
                        is_webtvos = false;
                    }

                    if at_string.as_str().contains("V1") { // Verbose results on
                        send_long_result = true;
                    } else if at_string.as_str().contains("V0") { // Verbose results off
                        send_long_result = false;
                    }

                    if at_string.as_str().contains("E1") { // echo mode on
                        echo_command = true;
                    } else if at_string.as_str().contains("E0") { // echo mode off
                        echo_command = false;
                    }

                    if at_string.contains("I3") { // Firmware info (56k modems only)
                        log::info!("[{mame_socket_address}] They think we're a 56k modem so turning 56k on!");
                        is_56k_modem = true;
                        is_56k_connect = true;
                        if let Err(e) = send_result(&mut mame, b"V69420_WEBTV-K56_DLP", false, true).await {
                            log::error!("Can't talk to MAME: error={e}");
                            return;
                        }
                    // DT or DP in the string means a dial command.
                    } else if at_string.contains("DT") || at_string.contains("DP") { // Dial string
                        if at_string.contains("18006138199") || at_string.contains("18004653537") { // Dialing the 1800 number should never connect as 56k
                            is_56k_connect = false;
                        }

                        if !is_webtvos {
                            if let Err(e) = send_wince_connection_result(&mut mame, send_long_result, false).await {
                                log::error!("Can't talk to MAME: error={e}");
                                return;
                            }
                            
                            if let Err(e) = start_ppp_loop(&mut mame, &local_program_command, &remote_socket_address).await {
                                log::error!("Error in PPP loop: error={e}");
                                return;
                            }
                        } else {
                            if let Err(e) = send_result(&mut mame, b"0", send_long_result, false).await { // OK
                                log::error!("Can't talk to MAME: error={e}");
                                return;
                            }
                        }
                    // ATD standalone is the request to go into data mode.
                    } else if at_string.contains("TD\x0d") { // ATD, go into data mode
                        if let Err(e) = send_webtvos_connection_result(&mut mame, is_56k_modem && is_56k_connect, send_long_result, false).await {
                            log::error!("Can't talk to MAME: error={e}");
                            return;
                        }

                        if is_webtvos {
                            if let Err(e) = start_ppp_loop(&mut mame, &local_program_command, &remote_socket_address).await {
                                log::error!("Error in PPP loop: error={e}");
                                return;
                            }
                        }
                    // All other command strings
                    } else {
                        if let Err(e) = send_result(&mut mame, b"0", send_long_result, true).await { // OK
                            log::error!("Can't talk to MAME: error={e}");
                            return;
                        }
                    }
                    at_string = "".to_string();
                }
            }
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts: CmdOpts = CmdOpts::parse();

    env_logger::Builder::new()
        .filter_level(opts.verbosity.into())
        .init();

    match server_loop(&opts) {
        Ok(r) => r,
        Err(e) => return Err(e)
    };

    Ok(())
}