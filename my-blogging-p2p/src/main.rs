use libp2p::{
    core::upgrade,
    floodsub::{Floodsub, FloodsubEvent, Topic},
    futures::StreamExt,
    identity,
    mdns::{Mdns, MdnsEvent},
    mplex,
    noise::{Keypair, NoiseConfig, X25519Spec},
    swarm::{NetworkBehaviourEventProcess, Swarm, SwarmBuilder},
    tcp::TokioTcpConfig,
    NetworkBehaviour, PeerId, Transport,
};
use log::{error, info};
use std::io::{self, Write};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tokio::{fs, io::AsyncBufReadExt, sync::mpsc};
use bcrypt::{hash, verify, DEFAULT_COST};
use dialoguer::Input;
use std::process;

const STORAGE_FILE_PATH: &str = "./blog.json";

type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
type Blogs = Vec<Blog>;

static KEYS: Lazy<identity::Keypair> = Lazy::new(|| identity::Keypair::generate_ed25519());
static PEER_ID: Lazy<PeerId> = Lazy::new(|| PeerId::from(KEYS.public()));
static TOPIC: Lazy<Topic> = Lazy::new(|| Topic::new("blogs"));

#[derive(Debug, Serialize, Deserialize)]
struct Blog {
    id: usize,
    author: String,
    subject: String,
    body: String,
    public: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct User {
    username: String,
    password_hash: String,
}

impl User {
    fn new(username: String, password: String) -> Result<Self, &'static str> {
        let hashed_password = hash_password(password)?;
        Ok(Self {
            username,
            password_hash: hashed_password,
        })
    }

    fn verify_password(&self, password: &str) -> bool {
        verify(password, &self.password_hash).unwrap_or(false)
    }
}

fn hash_password(password: String) -> Result<String, &'static str> {
    hash(password, DEFAULT_COST).map_err(|_| "Failed to hash password")
}



#[derive(Debug, Serialize, Deserialize)]
enum ListMode {
    ALL,
    One(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct ListRequest {
    mode: ListMode,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListResponse {
    mode: ListMode,
    data: Blogs,
    receiver: String,
}

enum EventType {
    Response(ListResponse),
    Input(String),
}

#[derive(NetworkBehaviour)]
struct BlogBehavior {
    floodsub: Floodsub,
    mdns: Mdns,
    #[behaviour(ignore)]
    response_sender: mpsc::UnboundedSender<ListResponse>,
}

impl NetworkBehaviourEventProcess<FloodsubEvent> for BlogBehavior {
    fn inject_event(&mut self, event: FloodsubEvent) {
        match event {
            FloodsubEvent::Message(msg) => {
                if let Ok(resp) = serde_json::from_slice::<ListResponse>(&msg.data) {
                    if resp.receiver == PEER_ID.to_string() {
                        info!("Response from {}:", msg.source);
                        resp.data.iter().for_each(|r| info!("{:?}", r));
                    }
                } else if let Ok(req) = serde_json::from_slice::<ListRequest>(&msg.data) {
                    match req.mode {
                        ListMode::ALL => {
                            info!("Received ALL req: {:?} from {:?}", req, msg.source);
                            respond_with_public_posts(
                                self.response_sender.clone(),
                                msg.source.to_string(),
                            );
                        }
                        ListMode::One(ref peer_id) => {
                            if peer_id == &PEER_ID.to_string() {
                                info!("Received req: {:?} from {:?}", req, msg.source);
                                respond_with_public_posts(
                                    self.response_sender.clone(),
                                    msg.source.to_string(),
                                );
                            }
                        }
                    }
                }
            }
            _ => (),
        }
    }
}

fn respond_with_public_posts(sender: mpsc::UnboundedSender<ListResponse>, receiver: String) {
    tokio::spawn(async move {
        match read_local_posts().await {
            Ok(posts) => {
                let resp = ListResponse {
                    mode: ListMode::ALL,
                    receiver,
                    data: posts.into_iter().filter(|r| r.public).collect(),
                };
                if let Err(e) = sender.send(resp) {
                    error!("error sending response via channel, {}", e);
                }
            }
            Err(e) => error!("error fetching local blogposts to answer ALL request, {}", e),
        }
    });
}

impl NetworkBehaviourEventProcess<MdnsEvent> for BlogBehavior {
    fn inject_event(&mut self, event: MdnsEvent) {
        match event {
            MdnsEvent::Discovered(discovered_list) => {
                for (peer, _addr) in discovered_list {
                    self.floodsub.add_node_to_partial_view(peer);
                }
            }
            MdnsEvent::Expired(expired_list) => {
                for (peer, _addr) in expired_list {
                    if !self.mdns.has_node(&peer) {
                        self.floodsub.remove_node_from_partial_view(&peer);
                    }
                }
            }
        }
    }
}

async fn create_new_post(author: &str, subject: &str, body: &str) -> Result<()> {
    let mut local_posts = read_local_posts().await?;
    let new_id = match local_posts.iter().max_by_key(|r| r.id) {
        Some(v) => v.id + 1,
        None => 0,
    };
    local_posts.push(Blog {
        id: new_id,
        author: author.to_owned(),
        subject: subject.to_owned(),
        body: body.to_owned(),
        public: false,
    });
    write_local_posts(&local_posts).await?;

    info!("Created blogpost:");
    info!("Author: {}", author);
    info!("Subject: {}", subject);
    info!("Body:: {}", body);

    Ok(())
}

async fn publish_post(id: usize) -> Result<()> {
    let mut local_posts = read_local_posts().await?;
    if let Some(post) = local_posts.iter_mut().find(|r| r.id == id) {
        post.public = true;
        write_local_posts(&local_posts).await?;
        info!("Published blogpost with id: {}", id);
        Ok(())
    } else {
        Err("Blog post not found".into())
    }
}

async fn read_local_posts() -> Result<Blogs> {
    let content = fs::read(STORAGE_FILE_PATH).await?;
    let result = serde_json::from_slice(&content)?;
    Ok(result)
}

async fn write_local_posts(posts: &Blogs) -> Result<()> {
    let json = serde_json::to_string(&posts)?;
    fs::write(STORAGE_FILE_PATH, &json).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();

    info!("Peer Id: {}", PEER_ID.clone());
    let (response_sender, mut response_rcv) = mpsc::unbounded_channel();

    let _username = rpassword::prompt_password_stdout("Create your username: ").unwrap();

    let password = rpassword::prompt_password_stdout("Create your password: ").unwrap();
    
    let user = match register_user() {
        Ok(user) => user,
        Err(err) => {
            error!("Failed to register user: {}", err);
            return;
        }
    };


    let auth_keys = Keypair::<X25519Spec>::new()
        .into_authentic(&KEYS)
        .expect("can create auth keys");

    let transp = TokioTcpConfig::new()
        .upgrade(upgrade::Version::V1)
        .authenticate(NoiseConfig::xx(auth_keys).into_authenticated())
        .multiplex(mplex::MplexConfig::new())
        .boxed();

    let mut behaviour = BlogBehavior {
        floodsub: Floodsub::new(PEER_ID.clone()),
        mdns: Mdns::new(Default::default())
            .await
            .expect("can create mdns"),
        response_sender,
    };

    behaviour.floodsub.subscribe(TOPIC.clone());

    let mut swarm = SwarmBuilder::new(transp, behaviour, PEER_ID.clone())
        .executor(Box::new(|fut| {
            tokio::spawn(fut);
        }))
        .build();

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    Swarm::listen_on(
        &mut swarm,
        "/ip4/0.0.0.0/tcp/0"
            .parse()
            .expect("can get a local socket"),
    )
    .expect("swarm can be started");

    if login_user(&user, &password) {
        println!("Welcome to Blogamania, {}!", user.username);
        println!("Post your first blog using create post!");
    } else {
        println!("Login failed. Incorrect username or password.");
        return;
    }

    loop {
        let evt = {
            tokio::select! {
                line = stdin.next_line() => Some(EventType::Input(line.expect("can get line").expect("can read line from stdin"))),
                response = response_rcv.recv() => Some(EventType::Response(response.expect("response exists"))),
                event = swarm.select_next_some() => {
                    info!("Unhandled Swarm Event: {:?}", event);
                    None
                },
            }
        };

        if let Some(event) = evt {
            match event {
                EventType::Response(resp) => {
                    let json = serde_json::to_string(&resp).expect("can jsonify response");
                    swarm
                        .behaviour_mut()
                        .floodsub
                        .publish(TOPIC.clone(), json.as_bytes());
                }
                EventType::Input(line) => match line.as_str() {
                    "discover peers" => handle_list_peers(&mut swarm).await,
                    cmd if cmd.starts_with("see posts") => handle_list_posts(cmd, &mut swarm).await,
                    cmd if cmd.starts_with("create post") => handle_create_post_prompt().await,
                    cmd if cmd.starts_with("publish post") => handle_publish_post(cmd).await,
                    cmd if cmd.starts_with("search posts") => handle_search_posts(&mut swarm).await,
                    cmd if cmd.starts_with("delete post") => handle_delete_post_prompt().await,
                    "logout" => handle_logout(&mut swarm).await,
                    _ => error!("unknown command"),
                },
            }
        }
    }

}

async fn handle_list_peers(swarm: &mut Swarm<BlogBehavior>) {
    info!("Discovered Peers:");
    let nodes = swarm.behaviour().mdns.discovered_nodes();
    let mut unique_peers = HashSet::new();
    for peer in nodes {
        unique_peers.insert(peer);
    }
    unique_peers.iter().for_each(|p| info!("{}", p));
}

async fn handle_list_posts(cmd: &str, swarm: &mut Swarm<BlogBehavior>) {
    let rest = cmd.strip_prefix("ls posts ");
    match rest {
        Some("all") => {
            let req = ListRequest {
                mode: ListMode::ALL,
            };
            let json = serde_json::to_string(&req).expect("can jsonify request");
            swarm
                .behaviour_mut()
                .floodsub
                .publish(TOPIC.clone(), json.as_bytes());
        }
        Some(posts_peer_id) => {
            let req = ListRequest {
                mode: ListMode::One(posts_peer_id.to_owned()),
            };
            let json = serde_json::to_string(&req).expect("can jsonify request");
            swarm
                .behaviour_mut()
                .floodsub
                .publish(TOPIC.clone(), json.as_bytes());
        }
        None => {
            match read_local_posts().await {
                Ok(v) => {
                    info!("Previously Posted Blogs! ({})", v.len());
                    v.iter().for_each(|r| info!("{:?}", r));
                }
                Err(e) => error!("error fetching previously posted blogposts: {}", e),
            };
        }
    };
}

async fn handle_create_post_prompt() {
    let author: String = Input::new().with_prompt("Enter author name").interact_text().unwrap();
    let subject: String = Input::new().with_prompt("Enter subject").interact_text().unwrap();
    let body: String = Input::new().with_prompt("Enter body").interact_text().unwrap();

    if let Err(e) = create_new_post(&author, &subject, &body).await {
        error!("error creating blogpost: {}", e);
    };
}
async fn handle_publish_post(cmd: &str) {
    if let Some(rest) = cmd.strip_prefix("publish r") {
        match rest.trim().parse::<usize>() {
            Ok(id) => {
                if let Err(e) = publish_post(id).await {
                    info!("error publishing blogpost with id {}, {}", id, e)
                } else {
                    info!("Published blogpost with id: {}", id);
                }
            }
            Err(e) => error!("invalid id: {}, {}", rest.trim(), e),
        };
    }
}
async fn handle_search_posts(_swarm: &mut Swarm<BlogBehavior>) {
    print!("Enter author's name to search");
    io::stdout().flush().unwrap();
    let author: String = Input::new()
    .with_prompt("Enter author's name to search")
    .interact_text()
    .unwrap();

    match read_local_posts().await {
        Ok(posts) => {
            let filtered_posts: Vec<&Blog> = posts.iter().filter(|post| post.author == author).collect();
            if filtered_posts.is_empty() {
                println!("No blogs found for author: {}", author);
            } else {
                println!("Blogs by author '{}':", author);
                for post in filtered_posts {
                    println!("{:?}", post);
                }
            }
        },
        Err(e) => error!("Error fetching previously posted blogposts: {}", e),
    }
}
async fn delete_post_prompt() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let id_to_delete: usize = Input::<String>::new()
        .with_prompt("Enter the ID of the post you want to delete:")
        .interact_text()?
        .parse()?;

    let mut local_posts: Blogs = read_local_posts().await?;
    
    if let Some(index) = local_posts.iter().position(|post| post.id == id_to_delete) {
        local_posts.remove(index);
        write_local_posts(&local_posts).await?;
        println!("Successfully deleted blog post with id: {}", id_to_delete);
        Ok(())
    } else {
        println!("Blog post with id {} not found.", id_to_delete);
        Ok(())
    }
}
async fn handle_delete_post_prompt() {
    if let Err(e) = delete_post_prompt().await {
        error!("Error deleting post: {}", e);
    }
}


fn register_user() -> Result<User, &'static str> {
    let username = rpassword::prompt_password_stdout("Enter your username: ").unwrap();
    let password = rpassword::prompt_password_stdout("Enter your password: ").unwrap();

    User::new(username, password)
}

fn login_user(user: &User, password: &str) -> bool {
    user.verify_password(password)
}

async fn logout(_swarm: &mut Swarm<BlogBehavior>) {
    println!("Logged out successfully.");
    process::exit(0);
}

async fn handle_logout(swarm: &mut Swarm<BlogBehavior>) {
    logout(swarm).await;
}