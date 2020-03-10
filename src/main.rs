use std::collections::{HashMap};
use std::convert::TryInto;
use std::env;
use std::error::Error;
use std::sync::{Arc};
use std::net::SocketAddr;

use futures::channel::oneshot;
use futures::SinkExt;

use regex::Regex;

use tokio;
use tokio::net::{TcpListener, TcpStream};
use tokio::io;
use tokio::io::AsyncWriteExt;
use tokio::stream::StreamExt;
use tokio::sync::Mutex;
use tokio_util::codec::{Framed, FramedRead, FramedWrite, LinesCodec, LinesCodecError};

const TOKEN_SIZE : usize = 64;
const SIDE_SIZE : usize = 16;

type BuddyReader = io::ReadHalf<TcpStream>;
type GlueChannel = (oneshot::Sender<BuddyReader>, oneshot::Receiver<BuddyReader>);
type BuddyChannel = oneshot::Sender<GlueChannel>;

type Token = [u8; TOKEN_SIZE];
type Side = [u8; SIDE_SIZE];

struct Server {
    waiting: HashMap<Token, HashMap<Side, BuddyChannel>>
}

impl Server {
    fn new() -> Self {
	Self {
	    waiting: HashMap::new(),
	}
    }

    fn add_connection(&mut self, token: Token, side: Side, new_ch: BuddyChannel) -> Result<(), Box<dyn Error>> {
	let old_conns = self.waiting.entry(token).or_insert(HashMap::new());
	let old_side = old_conns.keys().find(|k| **k != side).map(|k| k.clone());
	match old_side {
	    Some(old_side) => {
		let old_ch = old_conns.remove(&old_side).unwrap();
		let (new_send, new_recv) = oneshot::channel();
		let (old_send, old_recv) = oneshot::channel();
		new_ch.send((old_send, new_recv)).map_err(|_| "couldn't send to new conn")?;
		old_ch.send((new_send, old_recv)).map_err(|_| "couldn't send to old conn")?;
	    },
	    None => {
		old_conns.insert(side, new_ch);
	    }
	}
	Ok(())
    }
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {

    let addr = env::args().nth(1).unwrap_or_else(|| "[::]:4001".to_string());
    let mut listener = TcpListener::bind(&addr).await?;

    println!("server running on {}", addr);

    let server = Arc::new(Mutex::new(Server::new()));

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;

	let server = Arc::clone(&server);

        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
            if let Err(e) = process(server, stream, addr).await {
                println!("an error occurred; error = {:?}", e);
            }
        });
    }
}

async fn do_handshake(lines: &mut FramedRead<io::ReadHalf<TcpStream>, LinesCodec>) -> Result<(Token, Side), Box<dyn Error>> {
    let line = match lines.next().await {
	Some(Ok(line)) => line,
	_ => return Err("bad line".into())
    };

    println!("line: {}", line);
    
    let re = Regex::new(r"^please relay (?P<token>\w{32}) for (?P<side>\w{16})$")?;
    let caps = re.captures(&line).ok_or("bad handshake")?;

    let token : Token = caps["token"].as_bytes().try_into()?;
    let side : Side = caps["side"].as_bytes().try_into()?;
    Ok((token, side))
}


/// Process an individual chat client
async fn process(server: Arc<Mutex<Server>>, stream: TcpStream, addr: SocketAddr) -> Result<(), Box<dyn Error>> {
    let (my_rx, my_tx) = io::split(stream);
    let mut lines_rx = FramedRead::new(my_rx, LinesCodec::new());
    let mut lines_tx = FramedWrite::new(my_tx, LinesCodec::new());

    let handshake = do_handshake(&mut lines_rx).await.ok();
    let (token, side) = match handshake {
	Some(res) => res,
	None => { lines_tx.send("bad handshake").await?; return Err("bad handshake".into()) }
    };

    let (ch, buddy) = oneshot::channel();

    {
    	let mut server = server.lock().await;
    	server.add_connection(token, side, ch)?;
    }

    let (tx_ch, buddy_rx) = buddy.await?;
    let my_rx = lines_rx.into_inner();

    tx_ch.send(my_rx).map_err(|_| "couldn't send buddy our rx")?;
    let mut buddy_rx = buddy_rx.await?;
    lines_tx.send("ok").await?;

    let mut my_tx = lines_tx.into_inner();
    io::copy(&mut buddy_rx, &mut my_tx).await?; // copy from buddy's rx to our tx
    return Ok(())
}
