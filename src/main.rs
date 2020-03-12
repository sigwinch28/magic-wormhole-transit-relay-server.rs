use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::oneshot;

#[macro_use]
extern crate lazy_static;

use regex::{Regex, RegexBuilder};

use tokio::prelude::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::io::BufReader;
use tokio::sync::Mutex;
use tokio::time::timeout;

const OLD_HANDSHAKE_REGEX_TXT : &str = r"^please relay (?P<token>\w{64})$";
const NEW_HANDSHAKE_REGEX_TXT : &str = r"^please relay (?P<token>\w{64}) for side (?P<side>\w{16})$";

lazy_static! {
    static ref OLD_HANDSHAKE_REGEX : Regex = RegexBuilder::new(OLD_HANDSHAKE_REGEX_TXT)
	.size_limit(10 * (1 << 21))
	.build()
	.unwrap();

    static ref NEW_HANDSHAKE_REGEX : Regex = RegexBuilder::new(NEW_HANDSHAKE_REGEX_TXT)
	.size_limit(10 * (1 << 21))
	.build()
	.unwrap();
}

type Token = Vec<u8>;
type Side = Option<Vec<u8>>;

enum Buddy {
    NotHere(oneshot::Receiver<TcpStream>),
    Here(oneshot::Sender<TcpStream>),
}

struct Server {
    waiting: HashMap<Token, HashMap<Side, oneshot::Sender<TcpStream>>>,
}

impl Server {
    fn new() -> Self {
	Self { waiting: HashMap::new() }
    }

    fn add_connection(&mut self, token: Token, side: Side) -> Result<Buddy, Box<dyn Error>> {
	let old_conns = self.waiting.entry(token).or_insert(HashMap::new());
	let old_side = old_conns.keys().find(|k| **k != side || **k == None).map(|k| k.clone());
	match old_side {
	    Some(old_side) => {
		let old_ch = old_conns.remove(&old_side).unwrap();
		Ok(Buddy::Here(old_ch))
	    },
	    None => {
		let (send, recv) = oneshot::channel();
		old_conns.insert(side, send);
		Ok(Buddy::NotHere(recv))
	    }
	}
    }
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {

    let addr = env::args().nth(1).unwrap_or_else(|| "[::]:4001".to_string());
    let mut listener = TcpListener::bind(&addr).await?;

    println!("server running on {}", addr);

    let server : Arc<Mutex<Server>> = Arc::new(Mutex::new(Server::new()));

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;

	let server = Arc::clone(&server);

        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
	    if let Err(e) = process(server, stream).await {
		println!("error occurred with {}: {:?}", addr, e)	
	    }
        });
    }
}

async fn do_copy<R, T>(rx: &mut R, mut tx: &mut T) -> Result<u64, String>
where R : AsyncRead + Send + Unpin, T: AsyncWrite + Send + Unpin {
    let mut rx_buf = BufReader::new(rx);
    tx.write("ok\n".as_bytes()).await.map_err(|_| "couldn't send ok".to_string())?;
    println!("sent ok");
    let res = io::copy(&mut rx_buf, &mut tx).await.map_err(|_| "couldn't copy".to_string());
    tx.shutdown();
    res
}

async fn process(server: Arc<Mutex<Server>>, stream: TcpStream) -> Result<Option<(u64, u64)>, Box<dyn Error>> {
    stream.set_nodelay(true)?;
    let rx = BufReader::new(stream);
    
    let mut limited = rx.take(1024);
    let mut line = String::new();
    let handshake = || async {
	limited.read_line(&mut line).await?;
	
	if line.ends_with("\n") {
	    line.pop();
	}

	let res : Result<String, Box<dyn Error>> = Ok(line);
	res
    };

    let line = timeout(Duration::from_secs(60), handshake()).await??;
    println!("{}", line);

    let new_handshake = || { NEW_HANDSHAKE_REGEX.captures(&line) };
    let old_handshake = || { OLD_HANDSHAKE_REGEX.captures(&line) };

    let handshake = new_handshake().or_else(old_handshake).ok_or("bad handshake")?;
    let token : Token = handshake.name("token").unwrap().as_str().into();
    let side : Side = handshake.name("side").map(|s| s.as_str().into());
    
    let buddy;
    {
     	let mut server = server.lock().await;
    	buddy = server.add_connection(token, side)?;
    }

    let mut stream : TcpStream = limited.into_inner().into_inner();
    match buddy {
	Buddy::Here(ch) => {
	    ch.send(stream).map_err(|_| "couldn't send stream to buddy")?;
	    Ok(None)
	},
	Buddy::NotHere(ch) => {
	    let mut buddy_stream = ch.await?;
	    let (mut my_rx, mut my_tx) = stream.split();
	    let (mut buddy_rx, mut buddy_tx) = buddy_stream.split();

	    let (us_done_send, us_done_recv) = oneshot::channel();
	    let (buddy_done_send, buddy_done_recv) = oneshot::channel();

	    let buddy_to_us = || async move {
		tokio::select!{
		    nbytes = do_copy(&mut my_rx, &mut buddy_tx) => {
			us_done_send.send(()).map_err(|_| "couldn't send done")?;
			Ok(nbytes)
		    },
		    _ = buddy_done_recv => { Err("buddy disconnect") }
		}
	    };

	    let us_to_buddy = || async move {
		tokio::select!{
		    nbytes = do_copy(&mut buddy_rx, &mut my_tx) => {
			buddy_done_send.send(()).map_err(|_| "couldn't send done")?;
			Ok(nbytes)
		    },
		    _ = us_done_recv => { Err("buddy disconnected") }
		}
	    };
	    
	    //let buddy_to_us = do_copy(&mut my_rx, &mut buddy_tx);
	    //let us_to_buddy = do_copy(&mut buddy_rx, &mut my_tx);

	    tokio::try_join!(us_to_buddy(), buddy_to_us())
		.map(|(x,y)| (x.unwrap_or(0), y.unwrap_or(0)))
		.map(Some)
		.map_err(|e| e.into())

		
	}
    }
}
