use criterion::{Criterion, criterion_group, criterion_main};
use tracing::Level;
use tuwunel::{Args, Server, runtime};
use tuwunel_core::result::ErrLog;

criterion_group!(
	name = benches;
	config = Criterion::default().sample_size(10).nresamples(1);
	targets = dummy, smoke
);

criterion_main!(benches);

fn dummy(c: &mut Criterion) { c.bench_function("dummy", |c| c.iter(|| {})); }

fn smoke(c: &mut Criterion) {
	let args = Args::default_test(&["fresh", "cleanup"]);
	let runtime = runtime::new(Some(&args)).unwrap();
	let server = Server::new(Some(&args), Some(runtime.handle())).unwrap();
	runtime
		.block_on(async {
			tuwunel::async_start(&server).await?;
			let run = tuwunel::async_run(&server);
			c.bench_function("smoke", |c| {
				c.iter(|| {});
			});

			server.server.shutdown().log_err(Level::WARN).ok();
			run.await?;
			tuwunel::async_stop(&server).await
		})
		.unwrap();

	tuwunel::shutdown(&server, runtime).unwrap();
}
