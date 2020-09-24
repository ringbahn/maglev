use ringbahn::net::TcpListener;

use futures::StreamExt;
use futures::io::{AsyncReadExt, AsyncWriteExt};

fn main() {
    maglev::block_on(async {
        let mut listener = TcpListener::bind_on_driver("127.0.0.1:7878", maglev::driver()).unwrap();
        listener.incoming_no_addr().for_each_concurrent(16, |stream| async {
            let mut stream = stream.unwrap();
            let mut bytes = Vec::from(&b"echo> "[..]);
            let len = bytes.len();
            bytes.extend(&[0; 1024][..]);
            stream.write_all(b"Hello, world!\n").await.unwrap();
            loop {
                let n = stream.read(&mut bytes[len..]).await.unwrap();
                stream.write_all(&bytes[..(len + n)]).await.unwrap();
            }
        }).await;
    })
}
