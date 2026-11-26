//! Acquisition compiled out: no feature for anyone, and every call is `NotFound`.

use trackside_core::control::{
    AcquireControl, AcquireError, AcquireFeature, AcquireRequest, AcquisitionFilter,
    AcquisitionView, AdminAcquisitionView, ExternalQuery, ExternalSearch, InstantStream,
    ProviderChange, ProviderStatus, ProviderTest,
};
use trackside_core::{Access, AcquisitionId, BoxFuture, Page, Permissions};

#[derive(Debug, Default, Clone, Copy)]
pub struct NoAcquisition;

fn absent<'a, T: Send + 'a>() -> BoxFuture<'a, Result<T, AcquireError>> {
    Box::pin(async { Err(AcquireError::NotFound) })
}

impl AcquireControl for NoAcquisition {
    fn feature_for(&self, _: &Access, _: &Permissions, _: bool) -> Option<AcquireFeature> {
        None
    }
    fn search<'a>(
        &'a self,
        _: &'a Access,
        _: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }
    fn search_more<'a>(
        &'a self,
        _: &'a Access,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }
    fn request<'a>(
        &'a self,
        _: &'a Access,
        _: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }
    fn get<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }
    fn list<'a>(
        &'a self,
        _: &'a Access,
        _: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        absent()
    }
    fn cancel<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }
    fn instant_stream<'a>(
        &'a self,
        _: &'a Access,
        _: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<InstantStream, AcquireError>> {
        Box::pin(async { Err(AcquireError::NotFound) })
    }
    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>> {
        Box::pin(async { Vec::new() })
    }
    fn configure(&self, _: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>> {
        absent()
    }
    fn test_provider<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>> {
        absent()
    }
    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        absent()
    }
    fn retry<'a>(&'a self, _: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }
    fn admin_list(
        &self,
        _: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        Box::pin(async { Ok(Page::default()) })
    }
}
