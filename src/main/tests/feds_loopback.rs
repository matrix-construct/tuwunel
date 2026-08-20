#![cfg(all(test, feature = "direct_tls"))]

#[cfg(test)]
mod tests {

	use std::{
		env::var, fs::remove_dir_all, net::TcpListener, path::PathBuf, process::id as process_id,
		time::Duration,
	};

	use futures::future::join;
	use tokio::time::{sleep, timeout};
	use tuwunel::{Args, Runtime, Server, async_run, async_start, async_stop};
	use tuwunel_admin::{fini, init};
	use tuwunel_core::{Err, Result, err, ruma::ServerName};
	use tuwunel_service::Services;

	const CERTIFICATE: &str = "../../nix/pkgs/complement/certificate.crt";
	const PRIVATE_KEY: &str = "../../nix/pkgs/complement/private_key.key";
	const CLASS_HEADER: &str =
		"| rank | class | servers | name | version | commit | compiler | kernel | arch |";
	const ORIGIN_HEADER: &str = "| origin | class | elapsed | fault |";

	struct DatabasePath(PathBuf);

	impl Drop for DatabasePath {
		fn drop(&mut self) { remove_dir_all(&self.0).ok(); }
	}

	/// Exercises the wired version feds query through the local federation
	/// listener.
	///
	/// The parsed tables must account for the room's only destination exactly
	/// once.
	#[test]
	fn version_feds_query_reports_this_server() -> Result {
		let listener = TcpListener::bind(("127.0.0.1", 0))?;
		let port = listener.local_addr()?.port();
		let root = var("TMPDIR").unwrap_or_else(|_| "/nvme/target/tmp".into());
		let database = DatabasePath(
			PathBuf::from(root).join(format!("tuwunel-feds-query-{}", process_id())),
		);

		let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
		let certificate = source.join(CERTIFICATE);
		let private_key = source.join(PRIVATE_KEY);
		let mut args = Args::default_test(&["fresh", "cleanup"]);

		args.maintenance = true;
		args.option.extend([
			format!("database_path={:?}", database.0),
			format!("server_name=\"127.0.0.1:{port}\""),
			"address=[\"127.0.0.1\"]".to_owned(),
			format!("port={port}"),
			"listening=true".to_owned(),
			"ip_range_denylist=[]".to_owned(),
			"allow_invalid_tls_certificates=true".to_owned(),
			format!("tls.certs={certificate:?}"),
			format!("tls.key={private_key:?}"),
		]);

		let runtime = Runtime::new(Some(&args))?;
		let server = Server::new(Some(&args), Some(&runtime))?;
		let result = runtime.block_on(async {
			let services = async_start(&server).await?;
			let base = format!("https://127.0.0.1:{port}");

			drop(listener);

			let exercise = async {
				let outcome = exercise(&services, &base).await;
				let shutdown = server.server.shutdown();

				outcome.and(shutdown)
			};

			let (run, outcome) = join(async_run(&server), exercise).await;

			drop(services);
			async_stop(&server).await?;
			run?;

			outcome
		});

		drop(runtime);

		result
	}

	async fn exercise(services: &Services, base: &str) -> Result {
		wait_until_ready(services, base).await?;
		assert!(
			!services.config.federation_loopback,
			"the feds test must leave global loopback disabled",
		);
		init(&services.admin);

		let outcome = query_feds(services).await;

		fini(&services.admin);

		outcome
	}

	async fn wait_until_ready(services: &Services, base: &str) -> Result {
		let url = format!("{base}/_matrix/client/versions");

		timeout(Duration::from_secs(10), async {
			loop {
				if services
					.client
					.clients
					.default
					.get(&url)
					.send()
					.await
					.is_ok()
				{
					break;
				}

				sleep(Duration::from_millis(20)).await;
			}
		})
		.await
		.map_err(|_| err!("server listener did not become ready"))?;

		Ok(())
	}

	async fn query_feds(services: &Services) -> Result {
		let room_id = services.admin.get_admin_room().await?;
		let command = format!("query feds version {room_id}");
		let output = match services
			.admin
			.command_in_place(command, None)
			.await
		{
			| Ok(Some(output)) => output,
			| Ok(None) => return Err!("feds version query produced no output"),
			| Err(output) => return Err!("feds version query failed: {}", output.as_str()),
		};

		verify_output(output.as_str(), services.globals.server_name())
	}

	fn verify_output(output: &str, local: &ServerName) -> Result {
		let heading = output
			.lines()
			.find_map(|line| line.strip_prefix("Querying "))
			.ok_or_else(|| err!("feds output omitted its destination count"))?;

		let destinations: usize = heading
			.split_once(" servers in ")
			.map(|(count, _room_id)| count)
			.ok_or_else(|| err!("feds destination count was malformed"))?
			.parse()
			.map_err(|error| err!("feds destination count was not a number: {error}"))?;

		if destinations != 1 {
			return Err!("expected one feds destination, found {destinations}");
		}

		let class = only_row(output, CLASS_HEADER, "version class")?;
		let class_rank = parse_number(cell(class, 0)?, "class rank")?;
		let class_number = parse_number(cell(class, 1)?, "class number")?;
		let class_servers = parse_number(cell(class, 2)?, "class server count")?;
		let origin = only_row(output, ORIGIN_HEADER, "origin")?;
		let origin_name = cell(origin, 0)?;
		let origin_class = cell(origin, 1)?;
		let fault = cell(origin, 3)?;
		let successes = usize::from(!origin_class.is_empty());
		let faults = usize::from(!fault.is_empty());

		if successes.saturating_add(faults) != destinations {
			return Err!(
				"feds partition did not close: {successes} successes and {faults} faults for \
				 {destinations} destinations"
			);
		}

		if class_servers != successes {
			return Err!(
				"version class reports {class_servers} servers but {successes} origin rows \
				 reference it"
			);
		}

		if class_rank != 1 {
			return Err!("the local version class did not rank first");
		}

		if origin_name != local.as_str() {
			return Err!("expected local origin {local}, found {origin_name}");
		}

		if successes != 1 {
			return Err!("the local server did not return a version class");
		}

		if parse_number(origin_class, "origin class")? != class_number {
			return Err!("the local origin referenced a different version class");
		}

		Ok(())
	}

	fn only_row<'a>(output: &'a str, header: &str, name: &str) -> Result<&'a str> {
		let body = output
			.split_once(header)
			.map(|(_prefix, body)| body)
			.ok_or_else(|| err!("feds output omitted the {name} table"))?;

		let mut rows = body
			.lines()
			.skip(2)
			.take_while(|line| !line.trim().is_empty());

		let row = rows
			.next()
			.ok_or_else(|| err!("feds output omitted its {name} row"))?;

		if rows.next().is_some() {
			return Err!("feds output rendered more than one {name} row");
		}

		Ok(row)
	}

	fn parse_number(value: &str, field: &str) -> Result<usize> {
		value
			.parse()
			.map_err(|error| err!("feds {field} was not a number: {error}"))
	}

	fn cell(line: &str, index: usize) -> Result<&str> {
		let row = line
			.strip_prefix('|')
			.and_then(|line| line.strip_suffix('|'))
			.ok_or_else(|| err!("malformed feds table row: {line}"))?;

		row.split('|')
			.nth(index)
			.map(str::trim)
			.ok_or_else(|| err!("feds table row omitted cell {index}: {line}"))
	}
}
