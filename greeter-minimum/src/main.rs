use futures_util::stream::StreamExt;
use mio::net::TcpListener;

mod runtime;

async fn serve() {
    let mut listener = runtime::Async::<TcpListener>::new(proto::create_listen_socket());
    while let Some(ret) = listener.next().await {
        if let Ok(stream) = ret {
            runtime::spawn(async move {
                proto::client::Client::new(stream).serve().await;
            });
        }
    }
}

fn main() {
    runtime::run(serve);
}