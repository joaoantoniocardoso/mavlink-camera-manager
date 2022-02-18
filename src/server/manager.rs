use super::pages;

use actix_web::{
    error::{ErrorBadRequest, JsonPayloadError},
    rt::System,
    App, HttpRequest, HttpServer,
};
use paperclip::actix::{web, OpenApiExt};

use log::*;

fn json_error_handler(error: JsonPayloadError, _: &HttpRequest) -> actix_web::Error {
    warn!("Problem with json: {}", error.to_string());
    match error {
        JsonPayloadError::Overflow => JsonPayloadError::Overflow.into(),
        _ => ErrorBadRequest(error.to_string()),
    }
}

// Start REST API server with the desired address
pub fn run(server_address: &str) {
    let server_address = server_address.to_string();

    // Start HTTP server thread
    let _ = System::new("http-server");
    HttpServer::new(|| {
        App::new()
            .wrap_api()
            .with_json_spec_at("/docs.json")
            .with_swagger_ui_at("/docs")
            // Record services and routes for paperclip OpenAPI plugin for Actix.
            .data(web::JsonConfig::default().error_handler(json_error_handler))
            .route("/", web::get().to(pages::root))
            .route(r"/{filename:.*(\.html|\.js)}", web::get().to(pages::root))
            .route("/delete_stream", web::delete().to(pages::remove_stream))
            .route("/streams", web::get().to(pages::streams))
            .route("/streams", web::post().to(pages::streams_post))
            .route(
                "/streams/pipeline",
                web::post().to(pages::streams_custom_pipeline),
            )
            .route("/v4l", web::get().to(pages::v4l))
            .route("/v4l", web::post().to(pages::v4l_post))
            .route("/xml", web::get().to(pages::xml))
            .build()
    })
    .bind(server_address)
    .unwrap()
    .run();
}
