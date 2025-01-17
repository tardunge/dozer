use opentelemetry::{global, sdk::propagation::TraceContextPropagator};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

pub fn init_telemetry(enable_opel: bool) -> Result<(), Box<dyn ::std::error::Error>> {
    let app_name = "dozer";

    global::set_text_map_propagator(TraceContextPropagator::new());
    let tracer = opentelemetry_jaeger::new_agent_pipeline()
        .with_service_name(app_name)
        // .install_batch(opentelemetry::runtime::TokioCurrentThread)
        .install_simple()
        .expect("Failed to install OpenTelemetry tracer.");

    let fmt_layer = fmt::layer().with_target(false);
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    // Enable Open Telemetry
    let telemetry = match enable_opel {
        true => Some(tracing_opentelemetry::layer().with_tracer(tracer)),
        false => None,
    };

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(telemetry)
        .init();

    Ok(())
}
