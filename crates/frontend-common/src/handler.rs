use std::future::Future;
use std::pin::Pin;

pub(crate) type EventFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub(crate) trait EventHandler {
    type Context;
    type Output;

    fn handle<'a>(self, ctx: &'a mut Self::Context) -> EventFuture<'a, Self::Output>;
}
