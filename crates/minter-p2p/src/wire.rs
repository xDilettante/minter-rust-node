use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tendermint_proto::v0_34::p2p::{Packet, PacketMsg, PacketPong, packet::Sum};
use tendermint_std_ext::TryClone;

const PACKET_PAYLOAD: usize = 1024;
const MAX_PACKET_SIZE: usize = PACKET_PAYLOAD + 16;
// Nonblocking I/O preserves partial frame reads and permits socket shutdown on
// Windows without retrying a blocking operation after an OS timeout.
const IO_POLL: Duration = Duration::from_millis(10);
pub(crate) const MAX_BLOCK_MESSAGE: usize = 100 * 1024 * 1024 + 5;
pub(crate) const MAX_MEMPOOL_MESSAGE: usize = 1024 * 1024 + 8;

pub(crate) fn set_deadline(deadline: &Mutex<Instant>, timeout: Duration) -> Result<()> {
    *deadline
        .lock()
        .map_err(|_| anyhow::anyhow!("peer deadline lock"))? = Instant::now()
        .checked_add(timeout)
        .context("peer timeout is too large")?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Channel {
    RoundState = 0x20,
    ConsensusData = 0x21,
    ConsensusVote = 0x22,
    VoteSetBits = 0x23,
    Mempool = 0x30,
    Evidence = 0x38,
    Blocks = 0x40,
    Snapshots = 0x60,
    Chunks = 0x61,
}

impl Channel {
    fn max_message(self) -> usize {
        match self {
            Self::RoundState | Self::ConsensusData | Self::ConsensusVote | Self::VoteSetBits => {
                1024 * 1024
            }
            Self::Mempool => MAX_MEMPOOL_MESSAGE,
            Self::Evidence => crate::evidence::MAX_MESSAGE,
            Self::Blocks => MAX_BLOCK_MESSAGE,
            Self::Snapshots => 4_000_000,
            Self::Chunks => 16_000_000,
        }
    }

    pub fn is_consensus(self) -> bool {
        matches!(
            self,
            Self::RoundState | Self::ConsensusData | Self::ConsensusVote | Self::VoteSetBits
        )
    }
}

/// Apply one deadline across all ciphertext frames in a request.
pub(crate) struct DeadlineStream {
    stream: TcpStream,
    read_deadline: Arc<Mutex<Instant>>,
    write_deadline: Arc<Mutex<Instant>>,
}

impl DeadlineStream {
    pub fn new(
        stream: TcpStream,
        read_deadline: Arc<Mutex<Instant>>,
        write_deadline: Arc<Mutex<Instant>>,
    ) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            read_deadline,
            write_deadline,
        })
    }
}

impl TryClone for DeadlineStream {
    type Error = io::Error;

    fn try_clone(&self) -> io::Result<Self> {
        Self::new(
            self.stream.try_clone()?,
            self.read_deadline.clone(),
            self.write_deadline.clone(),
        )
    }
}

fn remaining(deadline: &Mutex<Instant>) -> io::Result<Duration> {
    let deadline = *deadline
        .lock()
        .map_err(|_| io::Error::other("peer deadline lock"))?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "peer deadline exceeded",
        ));
    }
    Ok(remaining)
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let wait = remaining(&self.read_deadline)?.min(IO_POLL);
            match self.stream.read(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(wait),
                result => return result,
            }
        }
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let wait = remaining(&self.write_deadline)?.min(IO_POLL);
            match self.stream.write(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(wait),
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

pub(crate) fn read_frame<T: Message + Default>(reader: &mut impl Read, limit: usize) -> Result<T> {
    let bytes = read_delimited(reader, limit)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of Tendermint stream",
        )
    })?;
    Ok(T::decode(bytes.as_slice())?)
}

pub(crate) fn read_delimited(reader: &mut impl Read, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut prefix = [0; 10];
    for index in 0..prefix.len() {
        match reader.read_exact(&mut prefix[index..=index]) {
            Err(error) if index == 0 && error.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(None);
            }
            result => result?,
        }
        if prefix[index] & 0x80 == 0 {
            let size = prost::decode_length_delimiter(&prefix[..=index])?;
            ensure!(
                size > 0 && size <= limit,
                "invalid Tendermint frame size {size}"
            );
            let mut bytes = vec![0; size];
            reader.read_exact(&mut bytes)?;
            return Ok(Some(bytes));
        }
    }
    bail!("invalid Tendermint frame length")
}

pub(crate) fn write_frame(writer: &mut impl Write, message: &impl Message) -> Result<()> {
    writer.write_all(&message.encode_length_delimited_to_vec())?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn pong(writer: &mut impl Write) -> Result<()> {
    write_frame(
        writer,
        &Packet {
            sum: Some(Sum::PacketPong(PacketPong {})),
        },
    )
}

pub(crate) fn send(
    writer: &mut impl Write,
    channel: Channel,
    message: &impl Message,
) -> Result<()> {
    send_encoded(writer, channel, &message.encode_to_vec())
}

pub(crate) fn send_encoded(
    writer: &mut impl Write,
    channel: Channel,
    encoded: &[u8],
) -> Result<()> {
    ensure!(
        !encoded.is_empty() && encoded.len() <= channel.max_message(),
        "invalid MConnection message size on {channel:?}"
    );
    let mut chunks = encoded.chunks(PACKET_PAYLOAD).peekable();
    while let Some(chunk) = chunks.next() {
        write_frame(
            writer,
            &Packet {
                sum: Some(Sum::PacketMsg(PacketMsg {
                    channel_id: i32::from(channel as u8),
                    eof: chunks.peek().is_none(),
                    data: chunk.to_vec(),
                })),
            },
        )?;
    }
    Ok(())
}

/// Reassembly persists across complete messages on other advertised channels.
pub(crate) struct Receiver(Vec<(Channel, Vec<u8>)>);

pub(crate) enum Incoming {
    Ping,
    Message(Channel, Vec<u8>),
    Pending,
}

impl Receiver {
    pub fn new(channels: &[Channel]) -> Self {
        Self(
            channels
                .iter()
                .map(|channel| (*channel, Vec::new()))
                .collect(),
        )
    }

    pub fn receive(
        &mut self,
        connection: &mut (impl Read + Write),
        limit: usize,
    ) -> Result<(Channel, Vec<u8>)> {
        loop {
            match self.next(connection, |_| limit)? {
                Incoming::Ping => pong(connection)?,
                Incoming::Message(channel, bytes) => return Ok((channel, bytes)),
                Incoming::Pending => {}
            }
        }
    }

    pub fn next(
        &mut self,
        connection: &mut impl Read,
        limit: impl FnOnce(Channel) -> usize,
    ) -> Result<Incoming> {
        let packet: Packet = read_frame(connection, MAX_PACKET_SIZE)?;
        match packet.sum.context("empty MConnection packet")? {
            Sum::PacketPing(_) => return Ok(Incoming::Ping),
            Sum::PacketPong(_) => {}
            Sum::PacketMsg(packet) => {
                let (channel, message) = self
                    .0
                    .iter_mut()
                    .find(|(channel, _)| i32::from(*channel as u8) == packet.channel_id)
                    .context("unexpected MConnection channel")?;
                ensure!(
                    !packet.data.is_empty() && packet.data.len() <= PACKET_PAYLOAD,
                    "invalid packet payload size"
                );
                ensure!(
                    message.len() + packet.data.len() <= channel.max_message().min(limit(*channel)),
                    "MConnection message too large on {channel:?}"
                );
                message.extend_from_slice(&packet.data);
                if packet.eof {
                    return Ok(Incoming::Message(*channel, std::mem::take(message)));
                }
            }
        }
        Ok(Incoming::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_consensus::SigningKey;
    use std::io::Cursor;
    use std::net::TcpListener;
    use tendermint_p2p::secret_connection::{SecretConnection, Version};
    use tendermint_proto::v0_34::p2p::PacketPing;

    struct Socket {
        incoming: Cursor<Vec<u8>>,
        outgoing: Vec<u8>,
    }

    impl Read for Socket {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.incoming.read(bytes)
        }
    }

    impl Write for Socket {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.outgoing.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn socket(packets: impl IntoIterator<Item = Sum>) -> Socket {
        let mut incoming = Vec::new();
        for sum in packets {
            write_frame(&mut incoming, &Packet { sum: Some(sum) }).unwrap();
        }
        Socket {
            incoming: Cursor::new(incoming),
            outgoing: Vec::new(),
        }
    }

    fn part(channel_id: u8, eof: bool, data: Vec<u8>) -> Sum {
        Sum::PacketMsg(PacketMsg {
            channel_id: i32::from(channel_id),
            eof,
            data,
        })
    }

    #[test]
    fn answers_ping_between_block_fragments() {
        let block = Channel::Blocks as u8;
        let mut connection = socket([
            part(block, false, vec![1, 2]),
            Sum::PacketPing(PacketPing {}),
            part(block, true, vec![3, 4]),
        ]);
        let mut reader = Receiver::new(&[Channel::Blocks]);
        assert_eq!(
            reader.receive(&mut connection, usize::MAX).unwrap(),
            (Channel::Blocks, vec![1, 2, 3, 4])
        );
        assert_eq!(
            connection.outgoing,
            Packet {
                sum: Some(Sum::PacketPong(PacketPong {}))
            }
            .encode_length_delimited_to_vec()
        );
    }

    #[test]
    fn rejects_oversized_frames_and_unadvertised_channels() {
        assert!(read_frame::<Packet>(&mut &[65u8][..], 64).is_err());
        assert!(read_frame::<Packet>(&mut &[0xffu8; 10][..], 64).is_err());
        let mut reader = Receiver::new(&[Channel::Blocks]);
        assert!(
            reader
                .receive(&mut socket([part(0x20, true, vec![1])]), usize::MAX)
                .is_err()
        );
        assert!(
            reader
                .receive(
                    &mut socket([part(
                        Channel::Blocks as u8,
                        true,
                        vec![1; PACKET_PAYLOAD + 1]
                    )]),
                    usize::MAX,
                )
                .is_err()
        );
    }

    #[test]
    fn keeps_chunk_fragments_across_snapshot_advertisements() {
        let mut connection = socket([
            part(Channel::Chunks as u8, false, vec![1, 2]),
            part(Channel::Snapshots as u8, true, vec![3]),
            part(Channel::Chunks as u8, true, vec![4, 5]),
        ]);
        let mut reader = Receiver::new(&[Channel::Snapshots, Channel::Chunks]);
        assert_eq!(
            reader.receive(&mut connection, usize::MAX).unwrap(),
            (Channel::Snapshots, vec![3])
        );
        assert_eq!(
            reader.receive(&mut connection, usize::MAX).unwrap(),
            (Channel::Chunks, vec![1, 2, 4, 5])
        );
    }

    #[test]
    fn short_io_waits_preserve_delayed_data_and_the_overall_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            accepted.write_all(b"ok").unwrap();
            let _ = wait.recv();
        });
        let deadline = Arc::new(Mutex::new(Instant::now() + Duration::from_secs(2)));
        let mut stream = DeadlineStream::new(stream, deadline.clone(), deadline.clone()).unwrap();
        let mut bytes = [0; 2];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ok");
        set_deadline(&deadline, Duration::from_millis(200)).unwrap();
        let began = Instant::now();
        assert_eq!(
            stream.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(began.elapsed() >= Duration::from_millis(150));
        assert!(began.elapsed() < Duration::from_secs(1));
        drop(release);
        sender.join().unwrap();
    }

    #[test]
    fn nonblocking_writes_preserve_bytes_when_the_peer_pauses_reading() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        let payload: Vec<u8> = (0_usize..16 * 1024 * 1024)
            .map(|i| (i.wrapping_mul(31) ^ (i >> 13)) as u8)
            .collect();
        let receiver = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            let mut received = Vec::new();
            accepted.read_to_end(&mut received).unwrap();
            received
        });
        let deadline = Arc::new(Mutex::new(Instant::now() + Duration::from_secs(5)));
        let mut stream = DeadlineStream::new(stream, deadline.clone(), deadline).unwrap();
        stream.write_all(&payload).unwrap();
        drop(stream);
        assert_eq!(receiver.join().unwrap(), payload);
    }

    #[test]
    fn encrypted_stream_retains_unread_frame_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let connect = |stream: TcpStream, key: u8| {
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            SecretConnection::new(stream, SigningKey::from([key; 32]), Version::V0_34).unwrap()
        };
        let sender = std::thread::spawn(move || {
            let mut stream = connect(listener.accept().unwrap().0, 1);
            stream.write_all(b"abc").unwrap();
            stream.write_all(b"defgh").unwrap();
        });
        let mut stream = connect(TcpStream::connect(address).unwrap(), 2);
        assert_eq!(stream.read(&mut []).unwrap(), 0);
        let mut prefix = [0; 1];
        stream.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"a");
        let mut buffer = [0; 16];
        assert_eq!(stream.read(&mut buffer).unwrap(), 2);
        assert_eq!(&buffer[..2], b"bc");
        stream.read_exact(&mut buffer[..5]).unwrap();
        assert_eq!(&buffer[..5], b"defgh");
        sender.join().unwrap();
    }
}
