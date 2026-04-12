pub(crate) fn linear_resample_into(
    output: &mut Vec<f32>,
    samples: &[f32],
    from_rate: u32,
    to_rate: u32,
) {
    if samples.is_empty() {
        output.clear();
        return;
    }

    if from_rate == to_rate {
        output.resize(samples.len(), 0.0);
        output.as_mut_slice().copy_from_slice(samples);
        return;
    }

    let ratio = to_rate as f64 / from_rate as f64;
    let output_len = (samples.len() as f64 * ratio) as usize;
    output.resize(output_len, 0.0);

    for (index, sample) in output.iter_mut().enumerate() {
        let source = index as f64 / ratio;
        let lower = source as usize;
        let fraction = (source - lower as f64) as f32;
        let s0 = samples[lower.min(samples.len() - 1)];
        let s1 = samples[(lower + 1).min(samples.len() - 1)];
        *sample = s0 + (s1 - s0) * fraction;
    }
}

#[cfg(test)]
mod tests {
    use super::linear_resample_into;

    #[test]
    fn preserves_identity_rate() {
        let input = vec![0.0, 0.5, -0.5, 1.0];
        let mut output = Vec::new();
        linear_resample_into(&mut output, &input, 16_000, 16_000);
        assert_eq!(output, input);
    }

    #[test]
    fn upsamples_with_linear_interpolation() {
        let mut output = Vec::new();
        linear_resample_into(&mut output, &[0.0, 1.0], 2, 4);
        assert_eq!(output, vec![0.0, 0.5, 1.0, 1.0]);
    }
}
