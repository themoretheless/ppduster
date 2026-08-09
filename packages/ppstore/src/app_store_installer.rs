//! Isolated macOS backend for starting Mac App Store downloads.
//!
//! Apple does not expose a public command-line installation API. On macOS we
//! therefore keep the small amount of private-framework integration in this
//! module and verify every runtime class before sending Objective-C messages.

use anyhow::{bail, Result};
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreOperation {
    /// Install an application that is already owned by the signed-in account.
    Install,
    /// Obtain a free application and install it.
    Get,
    /// Download the newest release of an installed application.
    Update,
}

impl StoreOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Get => "get",
            Self::Update => "update",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueResult {
    pub adam_id: u64,
    pub operation: StoreOperation,
    /// Whether CommerceKit confirmed the enqueue or the submitted request is
    /// still in an indeterminate state after the local wait timed out.
    #[serde(flatten)]
    pub status: QueueStatus,
    /// Confirmed downloads for [`QueueStatus::Queued`]. This is zero while the
    /// request is [`QueueStatus::Pending`] because no count was confirmed.
    pub downloads_queued: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum QueueStatus {
    Queued,
    Pending { detail: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallerBackendStatus {
    pub available: bool,
    pub backend: &'static str,
    pub detail: String,
}

/// Check whether the native installer backend is usable without changing
/// App Store state.
pub fn backend_status() -> InstallerBackendStatus {
    platform::backend_status()
}

/// Ask App Store services to enqueue a download.
///
/// A [`QueueStatus::Queued`] result means that the system accepted the request.
/// [`QueueStatus::Pending`] means that the request was submitted but the local
/// wait timed out before CommerceKit confirmed its outcome. The download and
/// final installation continue in Apple's background services and must be
/// verified by rescanning the application receipt/version before retrying.
pub fn queue(
    adam_id: u64,
    operation: StoreOperation,
    request_timeout: Duration,
) -> Result<QueueResult> {
    if adam_id == 0 {
        bail!("invalid App Store ADAM ID 0");
    }
    platform::queue(adam_id, operation, request_timeout)
}

#[cfg(any(target_os = "macos", test))]
fn pending_queue_result(
    adam_id: u64,
    operation: StoreOperation,
    request_timeout: Duration,
) -> QueueResult {
    QueueResult {
        adam_id,
        operation,
        status: QueueStatus::Pending {
            detail: format!(
                "App Store request was submitted but not confirmed within {request_timeout:?}; it may still be accepted, so rescan installed applications and available updates before retrying"
            ),
        },
        downloads_queued: 0,
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn backend_status() -> InstallerBackendStatus {
        InstallerBackendStatus {
            available: false,
            backend: "commerce-kit",
            detail: "Mac App Store installation is supported only on macOS".into(),
        }
    }

    pub fn queue(_: u64, _: StoreOperation, _: Duration) -> Result<QueueResult> {
        bail!("Mac App Store installation is supported only on macOS")
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use block2::RcBlock;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
    use objc2::{msg_send, sel};
    use objc2_foundation::{NSDate, NSError, NSRunLoop, NSString};
    use std::ffi::CStr;
    use std::sync::{mpsc, OnceLock};
    use std::time::Instant;

    const FRAMEWORKS: [&CStr; 2] = [
        c"/System/Library/PrivateFrameworks/CommerceKit.framework/CommerceKit",
        c"/System/Library/PrivateFrameworks/StoreFoundation.framework/StoreFoundation",
    ];
    static FRAMEWORK_LOAD: OnceLock<Result<(), String>> = OnceLock::new();

    const REQUIRED_CLASSES: [&std::ffi::CStr; 4] = [
        c"CKPurchaseController",
        c"SSPurchase",
        c"SSDownloadMetadata",
        c"NSArray",
    ];

    pub fn backend_status() -> InstallerBackendStatus {
        if let Err(detail) = load_frameworks() {
            return InstallerBackendStatus {
                available: false,
                backend: "commerce-kit",
                detail,
            };
        }
        let mut missing: Vec<String> = REQUIRED_CLASSES
            .iter()
            .filter(|name| AnyClass::get(name).is_none())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        if let Some(class) = AnyClass::get(c"SSPurchase") {
            if class
                .class_method(sel!(purchaseWithBuyParameters:))
                .is_none()
            {
                missing.push("+[SSPurchase purchaseWithBuyParameters:]".into());
            }
            for (selector, label) in [
                (sel!(setIsRedownload:), "-[SSPurchase setIsRedownload:]"),
                (sel!(setIsUpdate:), "-[SSPurchase setIsUpdate:]"),
                (sel!(setItemIdentifier:), "-[SSPurchase setItemIdentifier:]"),
                (
                    sel!(setDownloadMetadata:),
                    "-[SSPurchase setDownloadMetadata:]",
                ),
            ] {
                if class.instance_method(selector).is_none() {
                    missing.push(label.into());
                }
            }
        }
        if let Some(class) = AnyClass::get(c"SSDownloadMetadata") {
            for (selector, label) in [
                (sel!(initWithKind:), "-[SSDownloadMetadata initWithKind:]"),
                (
                    sel!(setItemIdentifier:),
                    "-[SSDownloadMetadata setItemIdentifier:]",
                ),
            ] {
                if class.instance_method(selector).is_none() {
                    missing.push(label.into());
                }
            }
        }
        if let Some(class) = AnyClass::get(c"CKPurchaseController") {
            if class.class_method(sel!(sharedPurchaseController)).is_none() {
                missing.push("+[CKPurchaseController sharedPurchaseController]".into());
            }
            if class
                .instance_method(sel!(performPurchase:withOptions:completionHandler:))
                .is_none()
            {
                missing.push(
                    "-[CKPurchaseController performPurchase:withOptions:completionHandler:]".into(),
                );
            }
        }
        if let Some(class) = AnyClass::get(c"NSArray") {
            if class.instance_method(sel!(count)).is_none() {
                missing.push("-[NSArray count]".into());
            }
        }
        if missing.is_empty() {
            InstallerBackendStatus {
                available: true,
                backend: "commerce-kit",
                detail: "required native App Store request interfaces are available; callback response selectors are validated before use".into(),
            }
        } else {
            InstallerBackendStatus {
                available: false,
                backend: "commerce-kit",
                detail: format!(
                    "missing private App Store runtime classes: {}",
                    missing.join(", ")
                ),
            }
        }
    }

    pub fn queue(
        adam_id: u64,
        operation: StoreOperation,
        request_timeout: Duration,
    ) -> Result<QueueResult> {
        let status = backend_status();
        if !status.available {
            bail!("App Store installer backend unavailable: {}", status.detail);
        }

        autoreleasepool(|_| unsafe {
            let purchase_class = required_class(c"SSPurchase")?;
            let metadata_class = required_class(c"SSDownloadMetadata")?;
            let controller_class = required_class(c"CKPurchaseController")?;

            let pricing_parameters = match operation {
                StoreOperation::Get => "STDQ&macappinstalledconfirmed=1",
                StoreOperation::Install | StoreOperation::Update => "STDRDL",
            };
            let buy_parameters = NSString::from_str(&format!(
                "productType=C&price=0&pg=default&appExtVrsId=0&pricingParameters={pricing_parameters}&salableAdamId={adam_id}"
            ));
            let kind = NSString::from_str("software");

            let purchase: *mut AnyObject = msg_send![
                purchase_class,
                purchaseWithBuyParameters: &*buy_parameters
            ];
            if purchase.is_null() {
                bail!("App Store failed to create a purchase request for {adam_id}");
            }
            let _: () = msg_send![purchase, setIsRedownload: operation != StoreOperation::Get];
            let _: () = msg_send![purchase, setIsUpdate: operation == StoreOperation::Update];
            let _: () = msg_send![purchase, setItemIdentifier: adam_id];

            let metadata: *mut AnyObject = msg_send![metadata_class, alloc];
            let metadata: *mut AnyObject = msg_send![metadata, initWithKind: &*kind];
            if metadata.is_null() {
                bail!("App Store failed to create download metadata for {adam_id}");
            }
            let _: () = msg_send![metadata, setItemIdentifier: adam_id];
            let _: () = msg_send![purchase, setDownloadMetadata: metadata];

            let controller: *mut AnyObject = msg_send![controller_class, sharedPurchaseController];
            if controller.is_null() {
                bail!("App Store purchase controller is unavailable");
            }

            let (sender, receiver) = mpsc::sync_channel::<Result<usize, String>>(1);
            let completion: RcBlock<dyn Fn(*mut AnyObject, Bool, *mut NSError, *mut AnyObject)> =
                RcBlock::new(
                    move |_: *mut AnyObject,
                          _: Bool,
                          error: *mut NSError,
                          response: *mut AnyObject| {
                        let result = {
                            if let Some(error) = error.as_ref() {
                                Err(error.localizedDescription().to_string())
                            } else {
                                checked_response_download_count(response)
                            }
                        };
                        let _ = sender.try_send(result);
                    },
                );

            let _: () = msg_send![
                controller,
                performPurchase: purchase,
                withOptions: 0_u64,
                completionHandler: &*completion
            ];

            let started = Instant::now();
            let run_loop = NSRunLoop::currentRunLoop();
            let result = loop {
                match receiver.try_recv() {
                    Ok(Ok(downloads_queued)) => {
                        break QueueResult {
                            adam_id,
                            operation,
                            status: QueueStatus::Queued,
                            downloads_queued,
                        };
                    }
                    Ok(Err(message)) => bail!(
                        "App Store rejected {} for {}: {}",
                        operation.as_str(),
                        adam_id,
                        message
                    ),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        bail!("App Store request channel closed unexpectedly")
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                if started.elapsed() >= request_timeout {
                    break pending_queue_result(adam_id, operation, request_timeout);
                }
                // Some CommerceKit callbacks are delivered through the
                // current thread's run loop rather than a global dispatch
                // queue, so pump it in short bounded slices.
                run_loop.runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
            };

            Ok(result)
        })
    }

    fn required_class(name: &'static std::ffi::CStr) -> Result<&'static AnyClass> {
        AnyClass::get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "App Store installer backend unavailable: missing runtime class {}",
                name.to_string_lossy()
            )
        })
    }

    pub(super) fn checked_response_download_count(
        response: *mut AnyObject,
    ) -> Result<usize, String> {
        let Some(response) = (unsafe { response.as_ref() }) else {
            return Err("App Store returned no download response".into());
        };
        if !responds_to_selector(response, sel!(downloads)) {
            return Err(
                "App Store response is missing the required -[response downloads] selector".into(),
            );
        }

        let downloads: *mut AnyObject = unsafe { msg_send![response, downloads] };
        checked_download_collection_count(downloads)
    }

    pub(super) fn checked_download_collection_count(
        downloads: *mut AnyObject,
    ) -> Result<usize, String> {
        let Some(downloads) = (unsafe { downloads.as_ref() }) else {
            return Err("App Store returned no downloads".into());
        };
        if !responds_to_selector(downloads, sel!(count)) {
            return Err(
                "App Store downloads object is missing the required -[downloads count] selector"
                    .into(),
            );
        }

        let count: usize = unsafe { msg_send![downloads, count] };
        if count == 0 {
            Err("App Store did not enqueue a download".into())
        } else {
            Ok(count)
        }
    }

    fn responds_to_selector(object: &AnyObject, selector: Sel) -> bool {
        unsafe { msg_send![object, respondsToSelector: selector] }
    }

    fn load_frameworks() -> Result<(), String> {
        FRAMEWORK_LOAD
            .get_or_init(|| {
                for path in FRAMEWORKS {
                    // Keep each system framework loaded for the lifetime of
                    // the process. This avoids a hard load command in every
                    // ppstore binary and lets unrelated commands keep working
                    // when Apple removes or renames a private framework.
                    let handle =
                        unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
                    if handle.is_null() {
                        let detail = unsafe {
                            let error = libc::dlerror();
                            if error.is_null() {
                                "unknown dynamic loader error".into()
                            } else {
                                CStr::from_ptr(error).to_string_lossy().into_owned()
                            }
                        };
                        return Err(format!(
                            "private App Store framework {} is unavailable: {}",
                            path.to_string_lossy(),
                            detail
                        ));
                    }
                }
                Ok(())
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_names_are_stable() {
        assert_eq!(StoreOperation::Install.as_str(), "install");
        assert_eq!(StoreOperation::Get.as_str(), "get");
        assert_eq!(StoreOperation::Update.as_str(), "update");
    }

    #[test]
    fn zero_adam_id_is_rejected_before_backend_access() {
        let error = queue(0, StoreOperation::Install, Duration::from_millis(1)).unwrap_err();
        assert!(error.to_string().contains("ADAM ID 0"));
    }

    #[test]
    fn timed_out_submission_has_stable_pending_status() {
        let queued_json = serde_json::to_value(QueueStatus::Queued).unwrap();
        assert_eq!(queued_json["status"], "queued");

        let result = pending_queue_result(
            497_799_835,
            StoreOperation::Install,
            Duration::from_secs(30),
        );

        assert_eq!(result.downloads_queued, 0);
        let QueueStatus::Pending { detail } = &result.status else {
            panic!("timed-out submission must remain pending");
        };
        assert!(detail.contains("request was submitted"));
        assert!(detail.contains("may still be accepted"));
        assert!(detail.contains("before retrying"));

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["status"], "pending");
        assert!(json["detail"].as_str().unwrap().contains("30s"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn backend_probe_is_read_only_and_does_not_panic() {
        let status = backend_status();
        assert_eq!(status.backend, "commerce-kit");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn callback_response_selectors_fail_closed() {
        use objc2::rc::autoreleasepool;
        use objc2::runtime::AnyObject;
        use objc2_foundation::NSString;

        autoreleasepool(|_| {
            let incompatible = NSString::from_str("not a CommerceKit response");
            let incompatible = (&*incompatible as *const NSString)
                .cast::<AnyObject>()
                .cast_mut();

            let response_error =
                platform::checked_response_download_count(incompatible).unwrap_err();
            assert!(response_error.contains("downloads] selector"));

            let collection_error =
                platform::checked_download_collection_count(incompatible).unwrap_err();
            assert!(collection_error.contains("count] selector"));
        });
    }
}
