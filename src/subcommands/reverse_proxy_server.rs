use argh::FromArgs;
use flume::Receiver;
use futures::TryFutureExt;
use quic_tunnel::compress::{copy_bidirectional_with_compression, CompressAlgo};
use quic_tunnel::counters::TunnelCounters;
use quic_tunnel::quic::{build_server_endpoint, CongestionMode};
use quic_tunnel::stream::Stream;
use quinn::Connecting;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket, UnixListener};
use tokio::select;
use tokio::time::timeout;
use tracing::{debug, error, info, trace};

/// Run the QUIC Tunnel Server.
#[derive(Debug, FromArgs, PartialEq)]
#[argh(subcommand, name = "reverse_proxy_server")]
pub struct ReverseProxyServerSubCommand {
    /// prefix for all the certificates to load
    #[argh(positional)]
    cert_name: String,

    /// the local address to listen on with QUIC. Clients connect here
    ///
    /// TODO: descriptive name
    #[argh(positional)]
    quic_addr: SocketAddr,

    /// the TCP address to bind. users that connect here will be forwarded to any clients connected to the QUIC address.
    #[argh(option)]
    tcp_listen: Option<SocketAddr>,

    /// the UDP address to bind. users that connect here will be forwarded to any clients connected to the QUIC address.
    #[argh(option)]
    udp_listen: Option<SocketAddr>,

    /// the Unix socket path to bind. users that connect here will be forwarded to any clients connected to the QUIC address.
    #[argh(option)]
    unix_listen: Option<PathBuf>,

    /// congestion mode for QUIC
    #[argh(option, default = "CongestionMode::NewReno")]
    congestion_mode: CongestionMode,

    /// compression mode for the QUIC tunnel.
    ///
    /// Be very careful with this! See: [CRIME](https://en.wikipedia.org/wiki/CRIME) attack!
    #[argh(option, default = "CompressAlgo::None")]
    compress: CompressAlgo,
}

impl ReverseProxyServerSubCommand {
    pub async fn main(self) -> anyhow::Result<()> {
        if self.tcp_listen.is_none() && self.unix_listen.is_none() {
            anyhow::bail!("specify tcp_listen or socket_listen or both");
        }

        let (stream_sender, stream_receiver) = flume::unbounded::<Stream>();

        let ca = PathBuf::new().join(format!("{}_ca.pem", self.cert_name));
        let cert = PathBuf::new().join(format!("{}_server.pem", self.cert_name));
        let key = PathBuf::new().join(format!("{}_server.key.pem", self.cert_name));

        let endpoint = build_server_endpoint(
            ca,
            cert,
            key,
            true,
            self.quic_addr,
            self.congestion_mode,
            false,
        )?;

        info!("QUIC listening on {}", endpoint.local_addr()?);

        let counts = TunnelCounters::new();

        // the tunnel handle listens on quic and forwards messages from a channel for tcp
        // TODO: better name
        let mut quic_endpoint_handle = {
            let endpoint = endpoint.clone();
            let stream_receiver = stream_receiver.clone();
            let compression_mode = self.compress;

            let f = async move {
                while let Some(conn) = endpoint.accept().await {
                    let f = handle_quic_connection(conn, stream_receiver.clone(), compression_mode);

                    // spawn to handle multiple connections at once? we only have one listener right now
                    tokio::spawn(f.inspect_err(|err| trace!(?err, "reverse proxy tunnel closed")));
                }
            };

            // this handle isn't needed. errors are logged elsewhere
            tokio::spawn(f)
        };

        // listens on tcp and forward all connections through a channel. any clients connected over quic will read the channel and handle the stream
        let mut tcp_listener_handle: tokio::task::JoinHandle<Result<(), anyhow::Error>> =
            if let Some(listen_addr) = self.tcp_listen {
                let stream_sender = stream_sender.clone();

                let f = async move {
                    // TODO: wait until at least one client has connected to the quic endpoint?

                    let tcp_listener = TcpListener::bind(listen_addr).await?;
                    info!("TCP listening on {}", tcp_listener.local_addr()?);

                    loop {
                        match tcp_listener.accept().await {
                            Ok((stream, _)) => {
                                // send the stream to a channel. one of multiple connections might handle it
                                stream_sender.send_async(Stream::Tcp(stream)).await?
                            }
                            Err(err) => error!(?err, "tcp accept failed"),
                        }
                    }
                };

                tokio::spawn(f.inspect_err(|err| trace!(?err, "tcp listener proxy closed")))
            } else {
                let f = std::future::pending::<anyhow::Result<()>>();

                tokio::spawn(f)
            };

        // listens on udp and forward all connections through a channel. any clients connected over quic will read the channel and handle the stream
        let mut udp_listener_handle: tokio::task::JoinHandle<Result<(), anyhow::Error>> =
            if let Some(listen_addr) = self.udp_listen {
                // let stream_sender = stream_sender.clone();

                let f = async move {
                    // TODO: wait until at least one client has connected to the quic endpoint?

                    let udp_socket = UdpSocket::bind(listen_addr).await?;
                    info!("UDP listening on {}", udp_socket.local_addr()?);

                    todo!("do we actually care about tunneling udp?");
                };

                tokio::spawn(f.inspect_err(|err| trace!(?err, "tcp listener proxy closed")))
            } else {
                let f = std::future::pending::<anyhow::Result<()>>();

                tokio::spawn(f)
            };

        // listens on unix socket and forward all connections through a channel. any clients connected over quic will read the channel and handle the stream
        let mut unix_listener_handle: tokio::task::JoinHandle<Result<(), anyhow::Error>> =
            if let Some(unix_listen_path) = self.unix_listen {
                let f = async move {
                    // TODO: wait until at least one client has connected to the quic endpoint?

                    info!("UNIX listening at {}", unix_listen_path.display());
                    let listener = UnixListener::bind(unix_listen_path)?;

                    loop {
                        match listener.accept().await {
                            Ok((stream, _)) => {
                                // send the stream to a channel. one of multiple connections might handle it
                                stream_sender.send_async(Stream::Unix(stream)).await?
                            }
                            Err(err) => error!(?err, "tcp accept failed"),
                        }
                    }
                };

                tokio::spawn(f.inspect_err(|err| trace!(?err, "tcp listener proxy closed")))
            } else {
                let f = std::future::pending::<anyhow::Result<()>>();

                tokio::spawn(f)
            };

        let mut stats_handle = counts.spawn_stats_loop();

        select! {
            x = &mut quic_endpoint_handle => {
                info!(?x, "tunnel task finished");
            }
            x = &mut tcp_listener_handle => {
                info!(?x, "tcp task finished");
            }
            x = &mut udp_listener_handle => {
                info!(?x, "udp task finished");
            }
            x = &mut unix_listener_handle => {
                info!(?x, "unix task finished");
            }
            x = &mut stats_handle => {
                info!(?x, "stats task finished");
            }
        }

        endpoint.close(0u32.into(), b"server done");

        quic_endpoint_handle.abort();
        tcp_listener_handle.abort();
        udp_listener_handle.abort();
        unix_listener_handle.abort();
        stats_handle.abort();

        Ok(())
    }
}

async fn handle_quic_connection(
    conn_a: Connecting,
    rx_b: Receiver<Stream>,
    compress_algo: CompressAlgo,
) -> anyhow::Result<()> {
    // TODO: are there other things I need to do to set up 0-rtt? this is copypasta
    let conn_a = match conn_a.into_0rtt() {
        Ok((conn_a, _)) => {
            trace!("0-rtt accepted");
            conn_a
        }
        Err(conn_a) => timeout(Duration::from_secs(30), conn_a).await??,
    };

    // TODO: look at the handshake data to figure out what client connected? that way we know what TcpListener to connect it to?

    loop {
        while let Ok(stream_b) = rx_b.recv_async().await {
            debug!(?stream_b, "user connected");

            // each new TCP stream gets a new QUIC stream
            let (tx_a, rx_a) = conn_a.open_bi().await?;

            trace!("reverse proxy stream opened");

            // TODO: counters while the stream happens
            let f = copy_bidirectional_with_compression(compress_algo, rx_a, tx_a, stream_b);

            // spawn to handle multiple requests at once
            tokio::spawn(
                f.inspect_err(|e| {
                    error!("failed: {}", e);
                })
                .inspect_ok(|(a_to_b, b_to_a)| trace!(%a_to_b, %b_to_a, "success")),
            );
        }
    }
}
