//! Tests for the hardware probe module.

#[cfg(test)]
mod tests {
    use phantom_gpu::probe;

    #[test]
    fn probe_never_panics() {
        // probe() should never fail, even without a GPU
        let result = probe::probe();
        // Just verify the struct is populated
        let _ = result.best_encoder();
        let _ = result.best_capture();
        let _ = result.has_gpu_pipeline();
    }

    #[test]
    fn probe_best_encoder_is_valid() {
        let result = probe::probe();
        let encoder = result.best_encoder();
        assert!(
            encoder == "nvenc" || encoder == "openh264",
            "unexpected encoder: {encoder}"
        );
        // If NVENC is detected, has_nvenc should be true
        if encoder == "nvenc" {
            assert!(result.has_nvenc);
            assert!(result.has_cuda);
        }
    }

    #[test]
    fn probe_best_capture_is_valid() {
        let result = probe::probe();
        let capture = result.best_capture();
        #[cfg(target_os = "linux")]
        assert!(
            capture == "nvfbc" || capture == "scrap",
            "unexpected capture: {capture}"
        );
        #[cfg(target_os = "windows")]
        assert!(
            capture == "dxgi" || capture == "scrap",
            "unexpected capture: {capture}"
        );
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        assert_eq!(capture, "scrap");
    }

    #[test]
    fn probe_gpu_pipeline_consistency() {
        let result = probe::probe();
        if result.has_gpu_pipeline() {
            // GPU pipeline requires both encoder and capture GPU support
            assert!(result.has_nvenc);
            #[cfg(target_os = "linux")]
            assert!(result.has_nvfbc);
            #[cfg(target_os = "windows")]
            assert!(result.has_dxgi);
        }
    }

    #[test]
    fn probe_no_nvenc_means_openh264() {
        // Can't easily mock, but verify the logic:
        // If has_nvenc is false, best_encoder must be openh264
        let result = probe::probe();
        if !result.has_nvenc {
            assert_eq!(result.best_encoder(), "openh264");
        }
    }

    #[test]
    fn probe_gpu_name_format() {
        let result = probe::probe();
        if let Some(ref name) = result.gpu_name {
            // GPU name should be non-empty and reasonable length
            assert!(!name.is_empty());
            assert!(name.len() < 200, "GPU name suspiciously long: {name}");
        }
    }
}
