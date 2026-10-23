//! The history request: timing samples and known targets per recipe and machine, bounded.

use crate::api::{
    ApiError, DEFAULT_HISTORY_SAMPLES, ErrorCode, HistoryView, MAX_HISTORY_RECIPES,
    MAX_HISTORY_SAMPLES, MAX_HISTORY_TOTAL_SAMPLES, MachineHistory, RecipeHistory, Response,
    SampleView, SeriesView,
};
use crate::scheduler::Scheduler;
use std::collections::BTreeMap;
use tasks_timing::SampleKey;

/// Series keys one request looks through; far above what the recipe bound allows for sane data.
const MAX_HISTORY_KEYS: usize = 20_000;

impl Scheduler {
    /// Samples and targets of `recipes` (all when empty) on `machines` (all when empty). The
    /// newest `max_samples` of each series are returned, oldest first; the whole reply holds at
    /// most [`MAX_HISTORY_TOTAL_SAMPLES`] samples and [`MAX_HISTORY_RECIPES`] recipes, and says
    /// `truncated` when a bound cut it.
    pub(crate) fn history_view(
        &self,
        recipes: &[String],
        machines: &[String],
        max_samples: Option<usize>,
    ) -> Result<Response, ApiError> {
        let per_series = max_samples.unwrap_or(DEFAULT_HISTORY_SAMPLES);
        if per_series == 0 || per_series > MAX_HISTORY_SAMPLES {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("max_samples must be between 1 and {MAX_HISTORY_SAMPLES}"),
            ));
        }
        let ledger_err = |e: &dyn std::fmt::Display| {
            ApiError::new(
                ErrorCode::LedgerUnavailable,
                format!("cannot read the timing history: {e}"),
            )
        };
        let mut keys = self
            .ledger
            .sample_keys(recipes, MAX_HISTORY_KEYS + 1)
            .map_err(|e| ledger_err(&e))?;
        let mut truncated = keys.len() > MAX_HISTORY_KEYS;
        keys.truncate(MAX_HISTORY_KEYS);
        keys.retain(|k| machines.is_empty() || machines.contains(&k.machine));

        // recipe -> machine -> (target, samples), in key order (recipe, machine, whole job first)
        type Series = Vec<(Option<String>, Vec<SampleView>)>;
        let mut grouped: BTreeMap<String, BTreeMap<String, Series>> = BTreeMap::new();
        let mut targets: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        let mut total = 0usize;
        for k in keys {
            if !grouped.contains_key(&k.recipe) && grouped.len() >= MAX_HISTORY_RECIPES {
                truncated = true;
                continue;
            }
            if let Some(t) = &k.target {
                targets
                    .entry((k.recipe.clone(), k.machine.clone()))
                    .or_default()
                    .push(t.clone());
            }
            let series = grouped
                .entry(k.recipe.clone())
                .or_default()
                .entry(k.machine.clone())
                .or_default();
            let room = MAX_HISTORY_TOTAL_SAMPLES - total;
            if room == 0 {
                truncated = true;
                continue;
            }
            let key = SampleKey {
                recipe: k.recipe.clone(),
                target: k.target.clone(),
                machine: k.machine.clone(),
            };
            let mut samples =
                tasks_timing::History::samples(&*self.ledger, &key).map_err(|e| ledger_err(&e))?;
            samples.sort_by_key(|s| s.at_ms);
            let keep = per_series.min(room);
            if samples.len() > keep {
                truncated = true;
            }
            let start = samples.len().saturating_sub(keep);
            let views: Vec<SampleView> = samples
                .get(start..)
                .unwrap_or(&[])
                .iter()
                .map(SampleView::of)
                .collect();
            total += views.len();
            series.push((k.target, views));
        }
        let recipes = grouped
            .into_iter()
            .map(|(recipe, by_machine)| RecipeHistory {
                machines: by_machine
                    .into_iter()
                    .map(|(machine, series)| MachineHistory {
                        targets: targets
                            .remove(&(recipe.clone(), machine.clone()))
                            .unwrap_or_default(),
                        machine,
                        series: series
                            .into_iter()
                            .map(|(target, samples)| SeriesView { target, samples })
                            .collect(),
                    })
                    .collect(),
                recipe,
            })
            .collect();
        Ok(Response::History(HistoryView { recipes, truncated }))
    }
}
