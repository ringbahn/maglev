maglev is a driver for ringbahn that is supposed to be fast. It may not be fast, or it may be badly
broken. I have not extensively tested it yet.

maglev provides a `Driver` type, which can be used as the driver for any ringbahn IO object, and a
function called `block_on`, which blocks the thread on a future that's been passed into it. It will
work the best by far to run a project using maglev in the `maglev::block_on` function.

maglev uses a thread-local instance of IoUring to submit IO events to the kernel. Every thread that
is processing tasks gets its own io-uring instance. Each thread submits work when it has events to
submit and no other work to do.
