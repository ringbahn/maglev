use ringbahn::*;
use futures::io::AsyncReadExt;

fn main() {
    maglev::block_on(async {
        let mut file: File<maglev::Driver> = File::open_on_driver("props.txt", maglev::Driver::default()).await.unwrap();
        let mut buf = vec![0; 4096];
        let end = file.read(&mut buf[..]).await.unwrap();

        buf.truncate(end);

        let (_, result) = maglev::Driver::default().submit(event::Write {
            fd: 1,
            buf: buf.into_boxed_slice(),
            offset: 0,
        }).await;

        result.unwrap();
    });
}
