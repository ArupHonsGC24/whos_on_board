use std::fs;
use std::time::Instant;
use std::io::Write;
use std::path::Path;
use chrono::NaiveDate;
use gtfs_structures::GtfsReader;

use raptor::network::Network;

use crate::simulation::{AgentCount, CrowdingCost, PopulationCount, SimulationParams, SimulationResult};
use crate::utils::create_pool;

mod simulation;
mod data_import;
mod data_export;
mod utils;

// Simulation notes:
// When we get the O-D data, we can run journey planning for each OD and apply the passenger counts to the relevant trips.
// once this is run once, we update the journey planning weights based on the crowding and run again.
// This is like the 'El Farol Bar' problem.
// Matsim-like replanning for a proportion of the population might also be viable.

pub struct DefaultSimulationParams {
    pub max_train_capacity: AgentCount,
}

impl DefaultSimulationParams {
    pub const fn new(max_train_capacity: AgentCount) -> Self {
        let result = Self {
            max_train_capacity,
        };

        result
    }
    fn f(x: CrowdingCost) -> CrowdingCost {
        const B: CrowdingCost = 5.;
        let bx = B * x;
        let ebx = bx.exp();
        (ebx - 1.) / (B.exp() - 1.)
    }
}

impl SimulationParams for DefaultSimulationParams {
    fn max_train_capacity(&self) -> AgentCount {
        self.max_train_capacity
    }

    fn cost_fn(&self, count: PopulationCount) -> CrowdingCost {
        debug_assert!(count >= 0, "Negative population count");
        let proportion = count as CrowdingCost / self.max_train_capacity() as CrowdingCost;
        Self::f(proportion)
    }
}

fn user_input(prompt: &str) -> Result<Option<String>, std::io::Error> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    input.truncate(input.trim_end().len());
    Ok(if input.is_empty() { None } else { Some(input) })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let exec_start = Instant::now();

    // Set up network.
    let network = {
        let gtfs_path = loop {
            let gtfs_path = user_input("Enter GTFS path (default ../gtfs/2/google_transit.zip): ")?;
            let gtfs_path = Path::new(gtfs_path.as_deref().unwrap_or("../gtfs/2/google_transit.zip"));

            if gtfs_path.exists() {
                let path = gtfs_path.to_string_lossy().to_string();
                println!("Reading GTFS from {path}.");
                break path;
            } else {
                println!("GTFS path {} does not exist.", gtfs_path.display());
            }
        };

        let gtfs_start = Instant::now();
        let gtfs = GtfsReader::default().read_from_path(gtfs_path)?;
        println!("GTFS import: {:?}", gtfs_start.elapsed());
        gtfs.print_stats();

        let journey_date = loop {
            let date_str = user_input("Which day to model (in 2024)? (DD/MM): ")?.unwrap_or(String::new());
            // Hardcode year to 2024.
            let date_str = format!("2024/{}", date_str.trim());
            match NaiveDate::parse_from_str(&date_str, "%Y/%d/%m") {
                Ok(parsed_date) => break parsed_date,
                Err(e) => {
                    println!("Invalid date format: {e:?}. Please try again.");
                }
            }
        };

        let default_transfer_time = 3 * 60;
        let network_start = Instant::now();
        let mut network = Network::new(&gtfs, journey_date, default_transfer_time);
        println!("Network parse: {:?}", network_start.elapsed());

        let connections_start = Instant::now();
        network.build_connections();
        println!("Build connections: {:?}", connections_start.elapsed());

        network.print_stats();

        network
    };

    // Set up simulation.
    let params = DefaultSimulationParams::new(
        // From VicSig: X'Trapolis 3-car has 264 seated, 133 standing. A 6-car has 794 in total.
        // Crush capacity is 1394, but that's a bit mean.
        // https://vicsig.net/suburban/train/X'Trapolis
        794,
    );

    loop {
        print!("Enter number of processors to use: ");
        std::io::stdout().flush()?;
        let mut num_procs = String::new();
        std::io::stdin().read_line(&mut num_procs)?;
        let num_processors = num_procs.trim().parse()?;
        // Set up thread pool for benchmarking.
        create_pool(num_processors)?.install(|| -> std::io::Result<()> {
            // Run simulation and print duration to csv.
            print!("Enter number of agents to use: ");
            std::io::stdout().flush()?;
            let mut num_agents = String::new();
            std::io::stdin().read_line(&mut num_agents)?;
            let num_agents = num_agents.trim().parse().unwrap();
            let simulation_steps = simulation::gen_simulation_steps(&network, Some(num_agents), Some(0));

            let mut simulation_result = SimulationResult { agent_journeys: Vec::new() };
            let simulation_start = Instant::now();
            let num_iterations = 1;
            for _ in 0..num_iterations {
                simulation_result = simulation::run_simulation::<_, true>(&network, &simulation_steps, &params);
            }
            let duration = simulation_start.elapsed() / (num_iterations * num_agents as u32);

            // Append to csv.
            if false {
                use std::fs::OpenOptions;
                use std::path::Path;

                let simulation_benchmark_path = "../data/simulation_scaling.csv";
                let exists = Path::new(simulation_benchmark_path).exists();
                let mut simulation_benchmark_file = OpenOptions::new().append(true).create(true).open("../data/simulation_benchmark.csv")?;
                if !exists {
                    writeln!(&mut simulation_benchmark_file, "num_processors,duration")?;
                }
                writeln!(&mut simulation_benchmark_file, "{num_processors},{}", duration.as_micros())?;

                println!("Simulation duration {} microseconds", duration.as_micros());
            }

            let data_export_folder = Path::new("../train_ute_export");
            println!("Exporting results to {}.", data_export_folder.display());
            let export_start = Instant::now();
            fs::create_dir_all(data_export_folder)?;
            data_export::export_agent_counts(&data_export_folder.join("counts"), &network, &simulation_result).unwrap();
            data_export::export_stops(&data_export_folder.join("stops"), &network).unwrap();
            if network.has_shapes {
                data_export::export_shape_file(&data_export_folder.join("shapes.bin.zip"), &network).unwrap();
                data_export::export_network_trips(&data_export_folder.join("trips.bin.zip"), &network, &simulation_result).unwrap();
            } else {
                println!("Warning: GTFS shapes not loaded, no visualisation export.");
            }
            println!("Export duration: {:?}", export_start.elapsed());

            println!();
            println!("Total time: {:?}", exec_start.elapsed());

            Ok(())
        })?;
    }
}
