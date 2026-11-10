use reknot_abi::services::*;
use reknot_abi::TaskHandle;

#[repr(C)]
struct Legacy {
    header: HandlerHeader,
    service: ServiceKey,
    make_request: MakeRequestFn,
    state_schema: ServiceKey,
    required_services: [ServiceRequirement; MAX_SERVICE_REQUIREMENTS],
    required_count: u32,
    init_schema: ServiceKey,
    validate_init: Option<ValidateInitFn>,
}
unsafe extern "C" fn make(_: *const HandlerRequest) -> TaskHandle {
    TaskHandle {
        data: std::ptr::null_mut(),
        vtable: std::ptr::null(),
    }
}
#[test]
fn frozen_abi5_prefix_is_read_without_touching_an_absent_metadata_tail() {
    assert_eq!(std::mem::size_of::<Legacy>(), HANDLER_API_V5_SIZE);
    let old = Legacy {
        header: HandlerHeader {
            abi_version: HANDLER_ABI_VERSION,
            struct_size: HANDLER_API_V5_SIZE as u32,
        },
        service: ServiceKey::new(101, 1),
        make_request: make,
        state_schema: ServiceKey::new(0, 0),
        required_services: [ServiceRequirement::EMPTY; MAX_SERVICE_REQUIREMENTS],
        required_count: 0,
        init_schema: ServiceKey::new(0, 0),
        validate_init: None,
    };
    let read = unsafe { HandlerApi::read_compatible((&old as *const Legacy).cast()) };
    assert_eq!(read.service, old.service);
    assert_eq!(read.metadata_version, 0);
    assert!(read.describe.is_none());
}
#[test]
fn unsupported_optional_metadata_version_is_not_interpreted() {
    unsafe extern "C" fn describe(_: *mut u8, _: usize) -> usize {
        5
    }
    let mut api = HandlerApi::new(ServiceKey::new(1, 1), make).with_metadata(describe);
    let read = unsafe { HandlerApi::read_compatible(&api) };
    assert_eq!(read.metadata_version, 1);
    assert!(read.describe.is_some());
    api.metadata_version = 99;
    let read = unsafe { HandlerApi::read_compatible(&api) };
    assert!(read.describe.is_none());
}
