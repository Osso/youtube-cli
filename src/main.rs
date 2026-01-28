mod api;
mod auth;
mod config;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "youtube")]
#[command(about = "CLI tool for YouTube Data API v3")]
struct Cli {
    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate with YouTube (opens browser)
    Login,
    /// Search for videos
    Search {
        /// Search query
        query: String,
        /// Maximum results
        #[arg(short = 'n', long, default_value = "10")]
        max: u32,
        /// Page token for pagination
        #[arg(long)]
        page: Option<String>,
    },
    /// Get video details
    Info {
        /// Video ID
        id: String,
    },
    /// Playlist operations
    #[command(subcommand)]
    Playlist(PlaylistCommands),
}

#[derive(Subcommand)]
enum PlaylistCommands {
    /// List your playlists
    List {
        #[arg(short = 'n', long, default_value = "25")]
        max: u32,
    },
    /// List videos in a playlist
    Items {
        /// Playlist ID
        id: String,
        #[arg(short = 'n', long, default_value = "50")]
        max: u32,
        /// Page token
        #[arg(long)]
        page: Option<String>,
    },
    /// Add videos to a playlist
    Add {
        /// Playlist ID
        playlist_id: String,
        /// Video IDs to add
        video_ids: Vec<String>,
    },
    /// Remove a video from a playlist (by playlist item ID)
    Remove {
        /// Playlist item ID (from `playlist items`)
        item_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Login => cmd_login().await,
        Commands::Search { query, max, page } => {
            cmd_search(&query, max, page.as_deref(), cli.json).await
        }
        Commands::Info { id } => cmd_info(&id, cli.json).await,
        Commands::Playlist(sub) => match sub {
            PlaylistCommands::List { max } => cmd_playlist_list(max, cli.json).await,
            PlaylistCommands::Items { id, max, page } => {
                cmd_playlist_items(&id, max, page.as_deref(), cli.json).await
            }
            PlaylistCommands::Add {
                playlist_id,
                video_ids,
            } => cmd_playlist_add(&playlist_id, &video_ids, cli.json).await,
            PlaylistCommands::Remove { item_id } => cmd_playlist_remove(&item_id).await,
        },
    }
}

async fn cmd_login() -> Result<()> {
    let config = config::load_config()?;
    let tokens = auth::login(config.client_id(), config.client_secret()).await?;
    println!("Logged in. Token saved to {:?}", config::tokens_path());
    drop(tokens);
    Ok(())
}

async fn cmd_search(query: &str, max: u32, page: Option<&str>, json: bool) -> Result<()> {
    let mut client = api::Client::new().await?;
    let result = client.search(query, max, page).await?;

    if json {
        print_json(&result.items)?;
        return Ok(());
    }

    for v in &result.items {
        println!(
            "{} | {} | {} | {} | {}",
            v.id,
            format_duration(&v.duration),
            v.views,
            v.channel,
            v.title,
        );
    }

    if let Some(token) = &result.next_page_token {
        eprintln!("\nNext page: --page {}", token);
    }
    Ok(())
}

async fn cmd_info(id: &str, json: bool) -> Result<()> {
    let mut client = api::Client::new().await?;
    let v = client.video_info(id).await?;

    if json {
        print_json(&v)?;
        return Ok(());
    }

    println!("Title:     {}", v.title);
    println!("Channel:   {}", v.channel);
    println!("Duration:  {}", format_duration(&v.duration));
    println!("Views:     {}", v.views);
    println!("Published: {}", v.published);
    println!("URL:       https://youtu.be/{}", v.id);
    if !v.description.is_empty() {
        println!("\n{}", v.description);
    }
    Ok(())
}

async fn cmd_playlist_list(max: u32, json: bool) -> Result<()> {
    let mut client = api::Client::new().await?;
    let playlists = client.list_playlists(max).await?;

    if json {
        print_json(&playlists)?;
        return Ok(());
    }

    for p in &playlists {
        println!("{} | {} videos | {}", p.id, p.count, p.title);
    }
    Ok(())
}

async fn cmd_playlist_items(id: &str, max: u32, page: Option<&str>, json: bool) -> Result<()> {
    let mut client = api::Client::new().await?;
    let result = client.playlist_items(id, max, page).await?;

    if json {
        print_json(&result.items)?;
        return Ok(());
    }

    for item in &result.items {
        println!(
            "{} | {} | {} | {}",
            item.video_id, item.playlist_item_id, item.channel, item.title,
        );
    }

    if let Some(token) = &result.next_page_token {
        eprintln!("\nNext page: --page {}", token);
    }
    Ok(())
}

async fn cmd_playlist_add(playlist_id: &str, video_ids: &[String], json: bool) -> Result<()> {
    let mut client = api::Client::new().await?;
    let added = client.playlist_add(playlist_id, video_ids).await?;

    if json {
        print_json(&added)?;
        return Ok(());
    }

    println!("Added {} videos to playlist {}", added.len(), playlist_id);
    Ok(())
}

async fn cmd_playlist_remove(item_id: &str) -> Result<()> {
    let mut client = api::Client::new().await?;
    client.playlist_remove(item_id).await?;
    println!("Removed playlist item {}", item_id);
    Ok(())
}

fn format_duration(iso: &str) -> String {
    let s = iso.trim_start_matches("PT");
    let mut hours = 0u32;
    let mut minutes = 0u32;
    let mut seconds = 0u32;
    let mut num = String::new();

    for c in s.chars() {
        match c {
            '0'..='9' => num.push(c),
            'H' => {
                hours = num.parse().unwrap_or(0);
                num.clear();
            }
            'M' => {
                minutes = num.parse().unwrap_or(0);
                num.clear();
            }
            'S' => {
                seconds = num.parse().unwrap_or(0);
                num.clear();
            }
            _ => {}
        }
    }

    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{}:{:02}", minutes, seconds)
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
