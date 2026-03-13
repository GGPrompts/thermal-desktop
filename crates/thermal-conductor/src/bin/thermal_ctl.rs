//! thermal-ctl — CLI for controlling thermal-conductor
//!
//! Usage:
//!   thermal-ctl create [command]    Create a new pane
//!   thermal-ctl send <pane> <keys>  Send keys to a pane
//!   thermal-ctl focus <pane>        Focus a pane
//!   thermal-ctl list                List all panes
//!   thermal-ctl state <pane>        Get agent state
//!   thermal-ctl layout <layout>     Set layout (grid/sidebar/stack)
//!   thermal-ctl capture <pane>      Capture pane content
//!   thermal-ctl kill <pane>         Kill a pane

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "thermal-ctl")]
#[command(about = "Control the Thermal Conductor agent dashboard")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new agent pane
    Create {
        /// Command to run in the pane
        #[arg(default_value = "zsh")]
        command: String,
    },
    /// Send keys to a pane
    Send {
        /// Pane ID (e.g., "pane-0" or "%0")
        pane: String,
        /// Keys to send
        keys: Vec<String>,
    },
    /// Focus a pane
    Focus {
        /// Pane ID
        pane: String,
    },
    /// List all panes and their states
    List,
    /// Get agent state for a pane
    State {
        /// Pane ID
        pane: String,
    },
    /// Set the layout
    Layout {
        /// Layout type: grid, sidebar, stack
        layout: String,
    },
    /// Capture pane content
    Capture {
        /// Pane ID
        pane: String,
        /// Number of scrollback lines
        #[arg(short, long, default_value = "50")]
        lines: i32,
    },
    /// Kill a pane
    Kill {
        /// Pane ID
        pane: String,
    },
}

fn main() {
    let cli = Cli::parse();

    // For now, use tmux directly. Later: D-Bus calls to thermal-conductor.
    match cli.command {
        Commands::Create { command } => {
            println!("◉ Creating pane with command: {}", command);
            // TODO: call tmux or D-Bus
            println!("  (D-Bus integration pending — use tmux directly for now)");
        }
        Commands::Send { pane, keys } => {
            let keys_str = keys.join(" ");
            println!("◉ Sending to {}: {}", pane, keys_str);
        }
        Commands::Focus { pane } => {
            println!("◉ Focusing pane: {}", pane);
        }
        Commands::List => {
            println!("◉ THERMAL CONDUCTOR — Pane Status");
            println!("  (D-Bus integration pending)");
        }
        Commands::State { pane } => {
            println!("◉ State for {}: unknown", pane);
        }
        Commands::Layout { layout } => {
            println!("◉ Setting layout: {}", layout);
        }
        Commands::Capture { pane, lines } => {
            println!("◉ Capturing {} ({} lines)", pane, lines);
        }
        Commands::Kill { pane } => {
            println!("◉ Killing pane: {}", pane);
        }
    }
}
