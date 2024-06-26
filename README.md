# WebTV Touch PPP

The WebTV MAME driver really wants to touch some PPP. WebTV tries to reach out but it just can't do it. This server gets things done so the WebTV can get down to business.

This might be considered a modem emulator but don't be fooled. The goal of this is to make the WebTV MAME driver connect and disconnect from a PPP server with reasonable (to me) reliability. This program provides no guarantee that the AT command set or RS232 flow control best practaces are aheared to or even supported.

After installing the [Rust compile tools](https://www.rust-lang.org/), you can compile touchppp with this command:

```sh
cargo build
```

For more information, you can read this guide: http://podsix.org/articles/pimodem/. This would replace the tcpser setup. Keep in mind that this program doesn't understand Telnet or IP232, so you might need to change the xinet.d setup on that PiModem setup page. In place of the `/etc/xinetd.d/pppd` file, you can use this:

```sh
service pppd
{
    type = UNLISTED
    flags = REUSE
    socket_type = stream
    wait = no
    user = root
    server = /usr/sbin/pppd
    server_args = notty noipv6
    disable = no
    bind = 127.0.0.1
    port = 2323
}
```

Alternatively, you can use socat to do the same thing:

```sh
socat tcp-l:2323,fork,reuseaddr exec:'/usr/sbin/pppd notty noipv6',pty,rawer,nonblock=1,iexten=0,b115200
```

Basic usage can be found by running `./touchppp --help`. Here's an example command line that will listen on 127.0.0.1:1122 and try to reach out to a server providing ppp at 127.0.0.1:2323.

```sh
touchppp -l 1122 -c 127.0.0.1:2323
```