use std::{collections::HashMap, sync::Arc};

use gpui::{
    App, AppContext as _, ImageCache, ImageCacheError, RenderImage, Resource, SharedString, Window,
};

pub(crate) struct MarkdownImageCache {
    images: HashMap<String, Arc<RenderImage>>,
}

impl MarkdownImageCache {
    pub(crate) fn new(cx: &mut App) -> gpui::Entity<Self> {
        cx.new(|_| Self {
            images: HashMap::new(),
        })
    }

    pub(crate) fn replace(&mut self, images: HashMap<String, Arc<RenderImage>>) {
        self.images = images;
    }
}

impl MarkdownImageCache {
    fn resolve(&self, resource: &Resource) -> Result<Arc<RenderImage>, ImageCacheError> {
        let key = match resource {
            Resource::Uri(uri) => uri.as_ref(),
            Resource::Path(_) | Resource::Embedded(_) => {
                return Err(ImageCacheError::Asset(SharedString::from(
                    "document image resource is not allowed",
                )));
            }
        };
        self.images
            .get(key)
            .cloned()
            .ok_or_else(|| ImageCacheError::Asset(SharedString::from("unknown document image")))
    }
}

impl ImageCache for MarkdownImageCache {
    fn load(
        &mut self,
        resource: &Resource,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
        Some(self.resolve(resource))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn unknown_resources_fail_closed() {
        let cache = MarkdownImageCache {
            images: HashMap::new(),
        };
        assert!(
            cache
                .resolve(&Resource::Uri("https://example.test/x.png".into()))
                .is_err()
        );
        assert!(
            cache
                .resolve(&Resource::Path(PathBuf::from("/tmp/x.png").into()))
                .is_err()
        );
        assert!(cache.resolve(&Resource::Embedded("x.png".into())).is_err());
    }
}
