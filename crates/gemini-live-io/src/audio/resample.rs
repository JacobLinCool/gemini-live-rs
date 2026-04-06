pub(crate) fn linear_resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if samples.is_empty() || from_rate == to_rate {
        return samples.to_vec();
    }

    let ratio = to_rate as f64 / from_rate as f64;
    let output_len = (samples.len() as f64 * ratio) as usize;

    (0..output_len)
        .map(|index| {
            let source = index as f64 / ratio;
            let lower = source as usize;
            let fraction = (source - lower as f64) as f32;
            let s0 = samples[lower.min(samples.len() - 1)];
            let s1 = samples[(lower + 1).min(samples.len() - 1)];
            s0 + (s1 - s0) * fraction
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::linear_resample;

    #[test]
    fn preserves_identity_rate() {
        let input = vec![0.0, 0.5, -0.5, 1.0];
        assert_eq!(linear_resample(&input, 16_000, 16_000), input);
    }

    #[test]
    fn upsamples_with_linear_interpolation() {
        let output = linear_resample(&[0.0, 1.0], 2, 4);
        assert_eq!(output, vec![0.0, 0.5, 1.0, 1.0]);
    }
}
