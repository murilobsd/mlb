#![allow(unused_variables, unused_imports, dead_code)]
#[macro_use]
extern crate log;
use clap::{
    crate_authors, crate_description, crate_name, crate_version, App,
    SubCommand,
};
use hyper::header::HeaderValue;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use hyper::{
    Body, Client, Method, Request, Response, Server as HTTPServer, StatusCode,
    Uri,
};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::{signal, spawn, sync::Mutex, time};

#[derive(Debug, Clone)]
pub struct Configuration {
    pub worker_pool_size: usize,
    pub frontend_config: Vec<FrontendConfig>,
}

#[derive(Debug, Clone)]
pub struct WorkerPool {
    workers: Vec<Worker>,
}

impl WorkerPool {
    pub fn new(conf: &Configuration) -> Self {
        debug!("new worker pool with {} workers", conf.worker_pool_size);
        let mut workers = Vec::with_capacity(conf.worker_pool_size);
        for _ in 0..conf.worker_pool_size {
            let worker = Worker::default();
            workers.push(worker);
        }

        Self { workers }
    }

    pub fn count_idle(&self) -> usize {
        debug!("count idle");
        self.workers.iter().filter(|&worker| worker.idle).count()
    }

    pub async fn get(
        &mut self,
        request: Arc<Request<Body>>,
        frontend: Frontend,
        tx: Arc<Sender<Response<Body>>>,
    ) {
        loop {
            if let Some(mut worker) =
                self.workers.iter_mut().find(|worker| worker.idle)
            {
                // @@@: create set_idler method
                worker.idle = false;
                worker
                    .run(request.clone(), frontend.clone(), tx.clone())
                    .await;
                // @@@: fix exit loop and change idle
                break;
            }

            time::sleep(Duration::from_millis(1)).await;
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Worker {
    idle: bool,
}

async fn exec_req(
    tx: Arc<Sender<Response<Body>>>,
    req: Arc<Request<Body>>,
    backend: Backend,
) {
    let url = format!("{}{}", "http://0.0.0.0:5001", req.uri());
    let client = Client::new();
    let req_backend = Request::builder()
        .method(req.method())
        .uri(&url)
        .header("User-Agent", "mlb/0.1.0")
        .body(Body::empty())
        .expect("request builder");
    debug!("exec_req req to backend: {:?} ", req_backend);

    let res = client.request(req_backend).await.unwrap();
    if res.status() == StatusCode::OK {
        debug!("exec_req ok")
    }
    if (tx.send(res).await).is_err() {
        println!("receiver dropped");
        return;
    }
    drop(tx);
}

impl Worker {
    pub fn new() -> Self {
        Self { idle: true }
    }

    // @@@: remove static frontend (dynamic)
    pub async fn run(
        &self,
        request: Arc<Request<Body>>,
        frontend: Frontend,
        tx: Arc<Sender<Response<Body>>>,
    ) {
        spawn(async move {
            // @@@: increment score backend
            let backend = frontend.backends[0].clone();
            exec_req(tx, request, backend).await;
        });
    }
}

impl Default for Worker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub name: String,
    pub address: String,
    pub heart_beat: String,
}

#[derive(Debug, Clone)]
pub struct BackendControl {
    failed: bool,
    active: bool,
    inactive_tries: usize,
    active_tries: usize,
    score: usize,
}

#[derive(Debug, Clone)]
pub struct Backend {
    control: Arc<Mutex<BackendControl>>,
}

impl Backend {
    pub fn new(conf: BackendConfig) -> Self {
        Self {
            control: Arc::new(Mutex::new(BackendControl {
                failed: true,
                active: false,
                inactive_tries: 0,
                active_tries: 0,
                score: 0,
            })),
        }
    }

    pub async fn health_check(&self) {
        spawn(async move {
            loop {
                let client = Client::new();
                let res = client
                    .get(Uri::from_static("http://0.0.0.0:5001/"))
                    .await
                    .unwrap();
                if res.status() == StatusCode::OK {
                    debug!("backend1 is ok")
                }

                time::sleep(Duration::from_secs(5)).await;
            }
        });
    }
}

#[derive(Debug, Clone)]
pub struct FrontendConfig {
    pub name: String,
    pub address: String,
    pub route: String,
    pub timeout: Duration,
    pub backend_config: Vec<BackendConfig>,
}

#[derive(Debug, Clone)]
pub struct Frontend {
    backends: Vec<Backend>,
}

impl Frontend {
    pub fn new(conf: FrontendConfig) -> Self {
        Self {
            backends: Vec::new(),
        }
    }
}

// @@@: add FrontendConfig to Frontend struct
// impl PartialEq for Frontend {
//     fn eq(&self, other: &Self) -> bool {
//         self.address == other.address
//     }
// }
//
// impl Eq for Frontend {}

pub struct Server {
    frontends: Vec<Frontend>,
    workerpool: WorkerPool,
}

#[derive(Clone)]
struct AppContext {
    frontend: Frontend,
    worker_pool: WorkerPool,
    tx: Arc<Sender<Response<Body>>>,
    rx: Arc<Mutex<Receiver<Response<Body>>>>,
}

// @@@: Remove this
async fn handle(
    context: AppContext,
    req: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    // @@@: Create a channel Receive response
    // @@@: Timeout (ticker)?
    //
    // recebido resposta no channel receive montar a resposta com o header da
    // resposta anterior.
    //
    // s.get -> workerpool.get -> worker.run
    //  - select backend
    //  - exec request
    debug!("Request Frontend: {:?}", req);
    let mut worker_pool = context.worker_pool;
    let request = Arc::new(req);
    worker_pool
        .get(request.clone(), context.frontend.clone(), context.tx)
        .await;

    if let Ok(i) = context.rx.lock().await.try_recv() {
        debug!("IN RECEICE RESPONSE");
        let status_code = i.status();
        let header = i.headers().clone();
        let buf = hyper::body::to_bytes(i).await.unwrap();
        // println!("got = {:?} Len: {}", buf, buf.len());
        let mut resp = Response::builder().status(status_code);
        let resp_header = resp.headers_mut().unwrap();
        for (key, value) in header.iter() {
            resp_header.insert(key, HeaderValue::from(value));
        }
        return Ok(resp.body(Body::from(buf)).unwrap());
    }

    Ok(Response::new(Body::from("Algum error aconteceu")))
}

impl Server {
    pub fn new(conf: &Configuration) -> Self {
        let mut frontends = Vec::new();
        // @@@: change clone() from frontend_config
        for frontend_config in conf.frontend_config.clone() {
            let mut frontend = Frontend::new(frontend_config.clone());
            for backend_config in frontend_config.backend_config {
                let backend = Backend::new(backend_config);
                frontend.backends.push(backend);
            }

            // @@@: Check if frontend exist
            frontends.push(frontend);
        }
        Self {
            frontends,
            workerpool: WorkerPool::new(conf),
        }
    }

    pub async fn run_frontend_server(&self, frontend: Frontend) {
        if frontend.backends.is_empty() {
            error!("no backend configuration detected");
            return;
        }

        for backend in &frontend.backends {
            backend.health_check().await;
        }

        // @@@: add FrontendConfig to Frontend struct
        // info!("Run frontend server [{}] at [{}]", frontend.name, frontend.address);
        let (tx, rx): (Sender<Response<Body>>, Receiver<Response<Body>>) =
            mpsc::channel(100);
        let context = AppContext {
            frontend,
            worker_pool: self.workerpool.clone(),
            tx: Arc::new(tx),
            rx: Arc::new(Mutex::new(rx)),
        };

        // A `MakeService` that produces a `Service` to handle each connection.
        let make_service = make_service_fn(move |conn: &AddrStream| {
            // We have to clone the context to share it with each invocation of
            // `make_service`. If your data doesn't implement `Clone` consider using
            // an `std::sync::Arc`.
            let context = context.clone();

            // You can grab the address of the incoming connection like so.
            let _addr = conn.remote_addr();

            // Create a `Service` for responding to the request.
            let service = service_fn(move |req| handle(context.clone(), req));

            // Return the service to hyper.
            async move { Ok::<_, Infallible>(service) }
        });

        // Construct our SocketAddr to listen on...
        let addr = SocketAddr::from(([0, 0, 0, 0], 5000));

        // And a MakeService to handle each connection...
        // let make_service = make_service_fn(|_conn| async {
        //     Ok::<_, Infallible>(service_fn(handle))
        // });

        // Then bind and serve...
        let server = HTTPServer::bind(&addr).serve(make_service);

        // And run forever...
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    }

    pub async fn run(&self) {
        for frontend in &self.frontends {
            self.run_frontend_server(frontend.clone()).await;
        }
    }

    fn stop(&self) {
        unimplemented!()
    }
}

pub async fn run_server(
    config: &Configuration,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Start MLB Server");
    info!("Using config file: ...");

    let server = Server::new(config);
    info!("Prepare to run server ...");
    server.run().await;

    println!("waiting for ctrl-c");

    signal::ctrl_c().await.expect("failed to listen for event");

    println!("received ctrl-c event");
    // server.stop();

    Ok(())
}

pub async fn create_app() -> Result<(), Box<dyn std::error::Error>> {
    // let matches = App::new(crate_name!())
    //     .version(crate_version!())
    //     .author(crate_authors!())
    //     .about(crate_description!())
    //     .args_from_usage(
    //         "-c, --config=[FILE] 'Sets a custom config file'
    //                                      <output> 'Sets an optional output file'
    //                                      -d... 'Turn debugging information on'",
    //     )
    //     .subcommand(
    //         SubCommand::with_name("test")
    //             .about("does testing things")
    //             .arg_from_usage("-l, --list 'lists test values'"),
    //     )
    //     .get_matches();

    // // You can check the value provided by positional arguments, or option arguments
    // if let Some(o) = matches.value_of("output") {
    //     println!("Value for output: {}", o);
    // }

    // if let Some(c) = matches.value_of("config") {
    //     println!("Value for config: {}", c);
    // }

    // // You can see how many times a particular flag or argument occurred
    // // Note, only flags can have multiple occurrences
    // match matches.occurrences_of("d") {
    //     0 => println!("Debug mode is off"),
    //     1 => println!("Debug mode is kind of on"),
    //     2 => println!("Debug mode is on"),
    //     3 | _ => println!("Don't be crazy"),
    // }

    // // You can check for the existence of subcommands, and if found use their
    // // matches just as you would the top level app
    // if let Some(matches) = matches.subcommand_matches("test") {
    //     // "$ myapp test" was run
    //     if matches.is_present("list") {
    //         // "$ myapp test -l" was run
    //         println!("Printing testing lists...");
    //     } else {
    //         println!("Not printing testing lists...");
    //     }
    // }

    // Continued program logic goes here...
    let conf = Configuration {
        worker_pool_size: 4,
        frontend_config: vec![FrontendConfig {
            name: String::from("frontend1"),
            address: String::from("0.0.0.0:5000"),
            route: String::from("/"),
            timeout: Duration::from_secs(5),
            backend_config: vec![
                BackendConfig {
                    name: String::from("backend1"),
                    address: String::from("0.0.0.0:5001"),
                    heart_beat: String::from("0.0.0.0:5001"),
                },
                BackendConfig {
                    name: String::from("backend2"),
                    address: String::from("0.0.0.0:5002"),
                    heart_beat: String::from("0.0.0.0:5002"),
                },
            ],
        }],
    };

    run_server(&conf).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_config() -> Configuration {
        let conf = Configuration {
            worker_pool_size: 4,
            frontend_config: vec![FrontendConfig {
                name: String::from("frontend1"),
                address: String::from("0.0.0.0:5000"),
                route: String::from("/"),
                timeout: Duration::from_secs(5),
                backend_config: vec![
                    BackendConfig {
                        name: String::from("backend1"),
                        address: String::from("0.0.0.0:5001"),
                        heart_beat: String::from("0.0.0.0:5001"),
                    },
                    BackendConfig {
                        name: String::from("backend2"),
                        address: String::from("0.0.0.0:5002"),
                        heart_beat: String::from("0.0.0.0:5002"),
                    },
                ],
            }],
        };
        conf
    }

    #[test]
    fn workerpool_count_idle_test() {
        let conf = simple_config();

        let worker_pool = WorkerPool::new(&conf);
        assert_eq!(worker_pool.count_idle(), 4);
    }
}
