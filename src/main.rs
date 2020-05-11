#[macro_use]
extern crate serde_json;
use libpulse_binding as pulse;

#[macro_use]
mod de;
#[macro_use]
mod util;
pub mod blocks;
mod config;
mod errors;
mod icons;
mod input;
mod scheduler;
mod subprocess;
mod themes;
mod widget;
mod widgets;

#[cfg(feature = "profiling")]
use cpuprofiler::PROFILER;

use std::collections::HashMap;
use std::ops::DerefMut;
use std::time::Duration;

use clap::{crate_authors, crate_description, crate_version, App, Arg, ArgMatches};
use crossbeam_channel::{select, Receiver, Sender};

use crate::blocks::create_block;
use crate::blocks::Block;
use crate::config::{load_config, Config};
use crate::errors::*;
use crate::input::{process_events, I3BarEvent};
use crate::scheduler::{Task, UpdateScheduler};
use crate::widget::{I3BarWidget, State};
use crate::widgets::text::TextWidget;

fn main() {
    let mut builder = App::new("i3status-rs")
        .version(crate_version!())
        .author(crate_authors!())
        .about(crate_description!())
        .arg(
            Arg::with_name("config")
                .value_name("CONFIG_FILE")
                .help("Sets a toml config file")
                .required(false)
                .index(1),
        )
        .arg(
            Arg::with_name("exit-on-error")
                .help("Exit rather than printing errors to i3bar and continuing")
                .long("exit-on-error")
                .takes_value(false),
        )
        .arg(
            Arg::with_name("one-shot")
                .help("Print blocks once and exit")
                .long("one-shot")
                .takes_value(false)
                .hidden(true),
        );

    if_debug!({
        builder = builder
            .arg(
                Arg::with_name("profile")
                    .long("profile")
                    .takes_value(true)
                    .help("A block to be profiled. Creates a `block.profile` file that can be analyzed with `pprof`"),
            )
            .arg(
                Arg::with_name("profile-runs")
                    .long("profile-runs")
                    .takes_value(true)
                    .default_value("10000")
                    .help("Number of times to execute update when profiling"),
            );
    });

    let matches = builder.get_matches();
    let exit_on_error = matches.is_present("exit-on-error");

    // Run and match for potential error
    if let Err(error) = run(&matches) {
        if exit_on_error {
            eprintln!("{:?}", error);
            ::std::process::exit(1);
        }

        let error_widget = TextWidget::new(Default::default())
            .with_state(State::Critical)
            .with_text(&format!("{:?}", error));
        let error_rendered = error_widget.get_rendered();
        println!(
            "{}",
            serde_json::to_string(&[error_rendered]).expect("failed to serialize error message")
        );

        eprintln!("\n\n{:?}", error);
        // Do nothing, so the error message keeps displayed
        loop {
            ::std::thread::sleep(Duration::from_secs(::std::u64::MAX));
        }
    }
}

#[allow(unused_mut)] // TODO: Remove when fixed in chan_select
fn run(matches: &ArgMatches) -> Result<()> {
    // Now we can start to run the i3bar protocol
    print!("{{\"version\": 1, \"click_events\": true}}\n[");

    // Read & parse the config file
    let config_path = match matches.value_of("config") {
        Some(config_path) => std::path::PathBuf::from(config_path),
        None => util::xdg_config_home().join("i3status-rust/config.toml"),
    };
    let config = load_config(&config_path)?;

    // Update request channel
    let (tx_update_requests, rx_update_requests): (Sender<Task>, Receiver<Task>) =
        crossbeam_channel::unbounded();

    // In dev build, we might diverge into profiling blocks here
    if let Some(name) = matches.value_of("profile") {
        profile_config(
            name,
            matches.value_of("profile-runs").unwrap(),
            &config,
            tx_update_requests,
        )?;
        return Ok(());
    }

    let mut config_alternating_tint = config.clone();
    {
        let tint_bg = &config.theme.alternating_tint_bg;
        config_alternating_tint.theme.idle_bg =
            util::add_colors(&config_alternating_tint.theme.idle_bg, tint_bg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.info_bg =
            util::add_colors(&config_alternating_tint.theme.info_bg, tint_bg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.good_bg =
            util::add_colors(&config_alternating_tint.theme.good_bg, tint_bg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.warning_bg =
            util::add_colors(&config_alternating_tint.theme.warning_bg, tint_bg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.critical_bg =
            util::add_colors(&config_alternating_tint.theme.critical_bg, tint_bg)
                .configuration_error("can't parse alternative_tint color code")?;

        let tint_fg = &config.theme.alternating_tint_fg;
        config_alternating_tint.theme.idle_fg =
            util::add_colors(&config_alternating_tint.theme.idle_fg, tint_fg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.info_fg =
            util::add_colors(&config_alternating_tint.theme.info_fg, tint_fg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.good_fg =
            util::add_colors(&config_alternating_tint.theme.good_fg, tint_fg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.warning_fg =
            util::add_colors(&config_alternating_tint.theme.warning_fg, tint_fg)
                .configuration_error("can't parse alternative_tint color code")?;
        config_alternating_tint.theme.critical_fg =
            util::add_colors(&config_alternating_tint.theme.critical_fg, tint_fg)
                .configuration_error("can't parse alternative_tint color code")?;
    }

    let mut blocks: Vec<Box<dyn Block>> = Vec::new();

    let mut alternator = false;
    // Initialize the blocks
    for &(ref block_name, ref block_config) in &config.blocks {
        blocks.push(create_block(
            block_name,
            block_config.clone(),
            if alternator {
                config_alternating_tint.clone()
            } else {
                config.clone()
            },
            tx_update_requests.clone(),
        )?);
        alternator = !alternator;
    }

    // We save the order of the blocks here,
    // because they will be passed to an unordered HashMap
    let order = blocks
        .iter()
        .map(|x| String::from(x.id()))
        .collect::<Vec<_>>();

    let mut scheduler = UpdateScheduler::new(&blocks);

    let mut block_map: HashMap<String, &mut dyn Block> = HashMap::new();

    for block in &mut blocks {
        block_map.insert(String::from(block.id()), (*block).deref_mut());
    }

    // We wait for click events in a separate thread, to avoid blocking to wait for stdin
    let (tx_clicks, rx_clicks): (Sender<I3BarEvent>, Receiver<I3BarEvent>) =
        crossbeam_channel::unbounded();
    process_events(tx_clicks);

    // Time to next update channel.
    // Fires immediately for first updates
    let mut ttnu = crossbeam_channel::after(Duration::from_millis(0));

    let one_shot = matches.is_present("one-shot");
    loop {
        // We use the message passing concept of channel selection
        // to avoid busy wait
        select! {
            // Receive click events
            recv(rx_clicks) -> res => if let Ok(event) = res {
                    for block in block_map.values_mut() {
                        block.click(&event)?;
                    }
                    util::print_blocks(&order, &block_map, &config)?;
            },
            // Receive async update requests
            recv(rx_update_requests) -> request => if let Ok(req) = request {
                // Process immediately and forget
                block_map
                    .get_mut(&req.id)
                    .internal_error("scheduler", "could not get required block")?
                    .update()?;
                util::print_blocks(&order, &block_map, &config)?;
            },
            // Receive update timer events
            recv(ttnu) -> _ => {
                scheduler.do_scheduled_updates(&mut block_map)?;
                // redraw the blocks, state changed
                util::print_blocks(&order, &block_map, &config)?;
            },
        }

        // Set the time-to-next-update timer
        match scheduler.time_to_next_update() {
            Some(time) => ttnu = crossbeam_channel::after(time),
            None => ttnu = crossbeam_channel::after(Duration::from_secs(std::u64::MAX)),
        }
        if one_shot {
            break Ok(());
        }
    }
}

#[cfg(feature = "profiling")]
fn profile(iterations: i32, name: &str, block: &mut dyn Block) {
    let mut bar = progress::Bar::new();
    println!(
        "Now profiling the {0} block by executing {1} updates.\n \
         Use pprof to analyze {0}.profile later.",
        name, iterations
    );

    PROFILER
        .lock()
        .unwrap()
        .start(format!("./{}.profile", name))
        .unwrap();

    bar.set_job_title("Profiling...");

    for i in 0..iterations {
        block.update().expect("block update failed");
        bar.reach_percent(((i as f64 / iterations as f64) * 100.).round() as i32);
    }

    PROFILER.lock().unwrap().stop().unwrap();
}

#[cfg(feature = "profiling")]
fn profile_config(name: &str, runs: &str, config: &Config, update: Sender<Task>) -> Result<()> {
    let profile_runs = runs
        .parse::<i32>()
        .configuration_error("failed to parse --profile-runs as an integer")?;
    for &(ref block_name, ref block_config) in &config.blocks {
        if block_name == name {
            let mut block =
                create_block(&block_name, block_config.clone(), config.clone(), update)?;
            profile(profile_runs, &block_name, block.deref_mut());
            break;
        }
    }
    Ok(())
}

#[cfg(not(feature = "profiling"))]
fn profile_config(_name: &str, _runs: &str, _config: &Config, _update: Sender<Task>) -> Result<()> {
    // TODO: Maybe we should just panic! here.
    Err(InternalError(
        "profile".to_string(),
        "The 'profiling' feature was not enabled at compile time.".to_string(),
        None,
    ))
}
