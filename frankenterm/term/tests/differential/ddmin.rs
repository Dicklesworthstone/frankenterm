//! Delta debugging: Zeller and Hildebrandt's ddmin over any list.

/// Shrinks `input` toward a 1-minimal list for which `fails` still holds.
/// `fails(&input)` must hold on entry. The result always satisfies `fails`;
/// it is 1-minimal unless `fails` was called `budget` times first.
pub fn ddmin<T: Clone>(
    input: Vec<T>,
    budget: usize,
    fails: &mut dyn FnMut(&[T]) -> bool,
) -> Vec<T> {
    let mut current = input;
    let mut granularity = 2;
    let mut calls = 0;
    while current.len() >= 2 && calls < budget {
        let len = current.len();
        let chunk = len.div_ceil(granularity);
        let mut reduced = false;

        // Reduce to one subset.
        let mut start = 0;
        while start < len && calls < budget {
            let end = (start + chunk).min(len);
            let subset = current[start..end].to_vec();
            calls += 1;
            if fails(&subset) {
                current = subset;
                granularity = 2;
                reduced = true;
                break;
            }
            start = end;
        }

        // Reduce to one complement; with two parts the complements are the
        // subsets just tried.
        if !reduced && granularity > 2 {
            let mut start = 0;
            while start < len && calls < budget {
                let end = (start + chunk).min(len);
                let complement: Vec<T> = current[..start]
                    .iter()
                    .chain(current[end..].iter())
                    .cloned()
                    .collect();
                calls += 1;
                if fails(&complement) {
                    current = complement;
                    granularity = (granularity - 1).max(2);
                    reduced = true;
                    break;
                }
                start = end;
            }
        }

        if !reduced {
            if granularity >= len {
                break;
            }
            granularity = (granularity * 2).min(len);
        }
    }
    current
}
