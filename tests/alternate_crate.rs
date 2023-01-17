// Copyright 2021 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

mod bonus {
    pub use metriken::*;
}

use bonus::Counter;

#[bonus::metric(name = "test", description = "foobar", crate = crate::bonus)]
static METRIC: Counter = Counter::new();

macro_rules! metric_in_macro {
    () => {
        #[$crate::bonus::metric(crate = $crate::bonus)]
        static OTHER_METRIC: Counter = Counter::new();
    };
}

metric_in_macro!();
