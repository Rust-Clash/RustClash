use clap::{Parser, Subcommand};
use common::{Addr, ProxyConnection, ProxyServer};
use config::{Inbound, Outbound};
use std::{io::Result as IOResult, path::Path, str::FromStr, sync::Arc};
use tokio::task::JoinHandle;
use uuid::Uuid;

mod config;
mod direct;
mod log;
mod reject;
mod route;
mod transport;

#[derive(Subcommand)]
enum Command {
    /** Run service */
    Run { config: String },
    /** Useful tools */
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
}

#[derive(Subcommand)]
enum ToolsCommand {
    /** Generate a UUID */
    Uuid,
}

#[derive(Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

/** forward data between inbound and outbound */
async fn handle_forwarding<I, O>(inbound: I, outbound: O) -> IOResult<()>
where
    I: ProxyConnection + Send + Unpin + 'static,
    O: ProxyConnection + Send + Unpin + 'static,
{
    let (inbound_read, inbound_write) = inbound.split();
    let (outbound_read, outbound_write) = outbound.split();
    let i_to_o: JoinHandle<IOResult<()>> = tokio::spawn(async move {
        let mut buf = vec![0; common::BUF_SIZE];
        loop {
            let (read_size, net) = inbound_read.receive(&mut buf).await?;
            /* connection is down */
            if read_size == 0 {
                return Ok(());
            }
            outbound_write.send(&buf[..read_size], net).await?;
        }
    });

    let o_to_i: JoinHandle<IOResult<()>> = tokio::spawn(async move {
        let mut buf = vec![0; common::BUF_SIZE];
        loop {
            let (read_size, net) = outbound_read.receive(&mut buf).await?;
            /* connection is down */
            if read_size == 0 {
                return Ok(());
            }
            inbound_write.send(&buf[..read_size], net).await?;
        }
    });

    loop {
        if o_to_i.is_finished() {
            i_to_o.abort();
            break;
        }
        if o_to_i.is_finished() {
            i_to_o.abort();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    Ok(())
}

fn to_sock_addr(host: &str, port: u16) -> String {
    /* is ipv6 address */
    if std::net::Ipv6Addr::from_str(host).is_ok() {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

async fn process_outbound<C>(
    outbound_config: Outbound,
    dst_addr: Addr,
    dst_port: u16,
    client: C,
) -> anyhow::Result<()>
where
    C: ProxyConnection + Send + Unpin + 'static,
{
    match outbound_config {
        Outbound::Direct { .. } => {
            let direct_connection =
                direct::DirectConnection::connect(dst_addr.to_socket_addr(dst_port)).await?;
            handle_forwarding(client, direct_connection).await?;
        }
        Outbound::Reject { .. } => {
            let reject_connection = reject::DirectConnection::default();
            handle_forwarding(client, reject_connection).await?;
        }
        Outbound::Socks5 { server, port, .. } => {
            let socks5_server =
                socks5::Socks5Client::connect(to_sock_addr(&server, port), dst_addr, dst_port)
                    .await?;

            handle_forwarding(client, socks5_server).await?;
        }
        Outbound::Trojan {
            server,
            port,
            password,
            tls,
            ..
        } => {
            use trojan::TrojanConnector;

            let mut trojan_connector = TrojanConnector::default();
            if let Some(tls) = tls {
                if let Some(insecure) = tls.insecure {
                    trojan_connector = trojan_connector.insecure(insecure);
                }
                if let Some(sni) = &tls.sni {
                    trojan_connector = trojan_connector.sni(sni);
                }
            }

            let trojan_server = trojan_connector
                .password(&password)
                .connect(&server, port, dst_addr, dst_port)
                .await?;

            handle_forwarding(client, trojan_server).await?;
        }
        Outbound::Shadowsocks {
            server,
            port,
            password,
            method,
            ..
        } => {
            use shadowsocks::{Method, ShadowsocksConnector};

            let cipher = Method::from_str(method.as_str()).unwrap();

            let ss_client = ShadowsocksConnector::default()
                .method(cipher)
                .password(&password)
                .connect(to_sock_addr(&server, port), dst_addr.clone(), dst_port)
                .await?;

            handle_forwarding(client, ss_client).await?;
        }
        Outbound::Vless {
            server,
            port,
            uuid,
            tls,
            ..
        } => {
            use vless::VlessConnector;

            let vless_builder = VlessConnector::default().uuid(Uuid::from_str(&uuid)?);

            if let Some(tls_config) = tls {
                let stream =
                    transport::connect_tls(to_sock_addr(&server, port), tls_config).await?;
                let vless_client = vless_builder
                    .connect(stream, dst_addr.clone(), dst_port)
                    .await?;

                handle_forwarding(client, vless_client).await?;
            } else {
                let stream = transport::connect_tcp(to_sock_addr(&server, port)).await?;
                let vless_client = vless_builder
                    .connect(stream, dst_addr.clone(), dst_port)
                    .await?;

                handle_forwarding(client, vless_client).await?;
            }
        }
        Outbound::Hysteria2 {
            server,
            port,
            password,
            tls,
            ..
        } => {
            use hysteria2::Hysteria2Connector;

            let mut hy2_connector = Hysteria2Connector::default().password(&password);
            if let Some(tls) = tls {
                if let Some(insecure) = tls.insecure {
                    hy2_connector = hy2_connector.insecure(insecure);
                }
                if let Some(sni) = &tls.sni {
                    hy2_connector = hy2_connector.sni(sni);
                }
            }

            let hy2_client = hy2_connector
                .connect(
                    to_sock_addr(&server, port),
                    &dst_addr.to_socket_addr(dst_port),
                )
                .await?;

            handle_forwarding(client, hy2_client).await?;
        }
    }

    Ok(())
}

async fn process_inbound(
    config: Inbound,
    outbounds: Vec<Outbound>,
    router: Arc<route::Router>,
) -> anyhow::Result<()> {
    /** call `ProxyServer::accept` to accept a connection and call `process_outbound` to start forwarding. */
    async fn handle_accept<S>(
        server: S,
        outbounds: &[Outbound],
        router: Arc<route::Router>,
    ) -> IOResult<()>
    where
        S: ProxyServer + Unpin + 'static,
    {
        let log = log::Log::default();
        loop {
            let client;
            let dst_addr;
            let dst_port;
            let src_addr;
            match server.accept().await {
                Ok((clt, (addr, port), src)) => {
                    client = clt;
                    dst_addr = addr;
                    dst_port = port;
                    src_addr = src;
                }
                Err(err) => {
                    log.log_error(err);
                    continue;
                }
            };

            let selected_outbound = router.get_outbound(dst_addr.clone().into());

            log.info(&format!(
                "[TCP] {} --> {} using {}",
                src_addr,
                dst_addr.to_socket_addr(dst_port),
                selected_outbound
            ));

            for outbound in outbounds {
                if outbound.get_name() == selected_outbound {
                    tokio::spawn(process_outbound(
                        outbound.clone(),
                        dst_addr,
                        dst_port,
                        client,
                    ));
                    break;
                }
            }
        }
    }

    match config {
        Inbound::Socks5 { listen, port, .. } => {
            let socks5_server = socks5::Socks5Server::listen(&listen, port).await?;

            handle_accept(socks5_server, &outbounds, router).await?;
        }
        Inbound::Trojan {
            listen,
            port,
            passwords,
            tls,
        } => {
            let mut trojan_builder = trojan::TrojanServerBuilder::default();
            for password in passwords {
                trojan_builder = trojan_builder.add_password(&password);
            }

            if let Some(file) = tls.cert_pam_file {
                trojan_builder = trojan_builder.add_cert_chain(&tokio::fs::read(file).await?);
            }
            if let Some(file) = tls.key_pam_file {
                trojan_builder = trojan_builder.add_key_der(&tokio::fs::read(file).await?);
            }

            let trojan_server = trojan_builder.listen(to_sock_addr(&listen, port)).await?;

            handle_accept(trojan_server, &outbounds, router).await?;
        }
        Inbound::Shadowsocks {
            listen,
            port,
            password,
            method,
        } => {
            use shadowsocks::{Method, ShadowsocksServer};
            let cipher = Method::from_str(method.as_str()).unwrap();

            let ss_server =
                ShadowsocksServer::bind(to_sock_addr(&listen, port), cipher, &password).await?;

            handle_accept(ss_server, &outbounds, router).await?;
        }
        Inbound::Vless {
            listen,
            port,
            uuids,
            ..
        } => {
            let mut vless_builder = vless::VlessServerBuilder::default();
            for uuid in uuids {
                vless_builder = vless_builder.add_uuid(Uuid::from_str(&uuid)?);
            }

            let vless_server = vless_builder.listen(to_sock_addr(&listen, port)).await?;

            handle_accept(vless_server, &outbounds, router).await?;
        }
    }
    Ok(())
}

async fn run_config<P>(config: P) -> anyhow::Result<()>
where
    P: AsRef<Path>,
{
    let config: config::Config =
        serde_yaml_ng::from_str(&tokio::fs::read_to_string(config).await?)?;
    let router = Arc::new(route::Router::from_config(&config));

    let mut handlers = Vec::new();

    for inbound in config.inbounds {
        let handle = tokio::spawn(process_inbound(
            inbound,
            config.outbounds.clone(),
            Arc::clone(&router),
        ));
        handlers.push(handle);
    }

    for handle in handlers {
        handle.await??;
    }

    Ok(())
}

fn rustls_set_default_provider() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    rustls_set_default_provider();

    match args.command {
        Command::Run { config } => run_config(config).await?,
        Command::Tools { command } => match command {
            ToolsCommand::Uuid => {
                println!("{}", Uuid::new_v4());
            }
        },
    }

    Ok(())
}
