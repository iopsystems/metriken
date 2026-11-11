/**
 * Unit formatting system for chart axes
 * Supports automatic scaling of values based on magnitude
 */

const UNIT_SYSTEMS = {
    // Time-based units (nanoseconds to seconds)
    time: {
        base: 'ns',
        scales: [{
            threshold: 0,
            suffix: 'ns',
            divisor: 1
        }, {
            threshold: 1000,
            suffix: 'μs',
            divisor: 1000
        }, {
            threshold: 1000000,
            suffix: 'ms',
            divisor: 1000000
        }, {
            threshold: 1000000000,
            suffix: 's',
            divisor: 1000000000
        }]
    },

    // Data size (bytes)
    bytes: {
        base: 'B',
        scales: [{
            threshold: 0,
            suffix: 'B',
            divisor: 1
        }, {
            threshold: 1024,
            suffix: 'KB',
            divisor: 1024
        }, {
            threshold: 1048576,
            suffix: 'MB',
            divisor: 1048576
        }, {
            threshold: 1073741824,
            suffix: 'GB',
            divisor: 1073741824
        }, {
            threshold: 1099511627776,
            suffix: 'TB',
            divisor: 1099511627776
        }]
    },

    // Network data rate (bits per second)
    bitrate: {
        base: 'bps',
        scales: [{
            threshold: 0,
            suffix: 'bps',
            divisor: 1
        }, {
            threshold: 1000,
            suffix: 'Kbps',
            divisor: 1000
        }, {
            threshold: 1000000,
            suffix: 'Mbps',
            divisor: 1000000
        }, {
            threshold: 1000000000,
            suffix: 'Gbps',
            divisor: 1000000000
        }, {
            threshold: 1000000000000,
            suffix: 'Tbps',
            divisor: 1000000000000
        }]
    },

    // Data rate (bits per second)
    datarate: {
        base: 'B/s',
        scales: [{
            threshold: 0,
            suffix: 'B/s',
            divisor: 1
        }, {
            threshold: 1000,
            suffix: 'KB/s',
            divisor: 1000
        }, {
            threshold: 1000000,
            suffix: 'MB/s',
            divisor: 1000000
        }, {
            threshold: 1000000000,
            suffix: 'GB/s',
            divisor: 1000000000
        }, {
            threshold: 1000000000000,
            suffix: 'TB/s',
            divisor: 1000000000000
        }]
    },

    // Percentage (already formatted, just add %)
    percentage: {
        base: '%',
        scales: [{
            threshold: 0,
            suffix: '%',
            divisor: 1,
            multiplier: 100
        }]
    },

    // Frequency (Hz to GHz)
    frequency: {
        base: 'Hz',
        scales: [{
            threshold: 0,
            suffix: 'Hz',
            divisor: 1
        }, {
            threshold: 1000,
            suffix: 'KHz',
            divisor: 1000
        }, {
            threshold: 1000000,
            suffix: 'MHz',
            divisor: 1000000
        }, {
            threshold: 1000000000,
            suffix: 'GHz',
            divisor: 1000000000
        }]
    },

    // Power (watts). Dashboard power plots derive watts from the microjoule
    // energy counters: irate() yields uJ/s (= uW), so the query divides by 1e6.
    power: {
        base: 'W',
        scales: [{
            // Sub-watt steps matter here: an idle integrated GPU draws
            // microwatts, and that domain is precisely why the dashboard
            // derives power from the energy counters instead of the
            // milliwatt gauges. Starting at `W` would render it `0 W` and
            // reintroduce the problem the derivation exists to avoid.
            //
            // `threshold: 0` must stay on the lowest scale: the selector walks
            // scales downward and falls back to `scales[0]`, so a value below
            // every threshold formats with whatever sits first.
            threshold: 0,
            suffix: 'uW',
            divisor: 0.000001
        }, {
            threshold: 0.001,
            suffix: 'mW',
            divisor: 0.001
        }, {
            threshold: 1,
            suffix: 'W',
            divisor: 1
        }, {
            threshold: 1000,
            suffix: 'KW',
            divisor: 1000
        }, {
            threshold: 1000000,
            suffix: 'MW',
            divisor: 1000000
        }]
    },

    // Count (no units, just numbers with K, M, B suffixes)
    count: {
        base: '',
        scales: [{
            threshold: 0,
            suffix: '',
            divisor: 1
        }, {
            threshold: 1000,
            suffix: 'K',
            divisor: 1000
        }, {
            threshold: 1000000,
            suffix: 'M',
            divisor: 1000000
        }, {
            threshold: 1000000000,
            suffix: 'B',
            divisor: 1000000000
        }]
    },

    // Rate (no units other than /s, just numbers with K/s, M/s, B/s suffixes)
    rate: {
        base: '',
        scales: [{
            threshold: 0,
            suffix: '/s',
            divisor: 1
        }, {
            threshold: 1000,
            suffix: 'K/s',
            divisor: 1000
        }, {
            threshold: 1000000,
            suffix: 'M/s',
            divisor: 1000000
        }, {
            threshold: 1000000000,
            suffix: 'B/s',
            divisor: 1000000000
        }]
    },

    // Temperature (degrees Celsius)
    temperature: {
        base: '°C',
        scales: [{
            threshold: 0,
            suffix: '°C',
            divisor: 1
        }]
    },

    // CPU cores (a count of cores, or core-seconds per second)
    cores: {
        base: 'cores',
        scales: [{
            threshold: 0,
            suffix: 'cores',
            divisor: 1
        }]
    }
};

/**
 * The units a series can be stored in (the storage key `unit`), each with
 * its dimension and what a value in it is multiplied by to be in that
 * dimension's base: nanoseconds, bytes, bits, a ratio (0 to 1), degrees
 * Celsius, hertz or cores.
 *
 * `percentage` is read as `percent` (0 to 100): rezolus samplers and
 * metriken-recorder 0.12–0.14 write that name.
 */
const STORED_UNITS = {
    nanoseconds: { dimension: 'time', factor: 1 },
    microseconds: { dimension: 'time', factor: 1e3 },
    milliseconds: { dimension: 'time', factor: 1e6 },
    seconds: { dimension: 'time', factor: 1e9 },
    bytes: { dimension: 'bytes', factor: 1 },
    kilobytes: { dimension: 'bytes', factor: 1e3 },
    kibibytes: { dimension: 'bytes', factor: 1024 },
    megabytes: { dimension: 'bytes', factor: 1e6 },
    mebibytes: { dimension: 'bytes', factor: 1048576 },
    gigabytes: { dimension: 'bytes', factor: 1e9 },
    gibibytes: { dimension: 'bytes', factor: 1073741824 },
    bits: { dimension: 'bits', factor: 1 },
    percent: { dimension: 'ratio', factor: 0.01 },
    percentage: { dimension: 'ratio', factor: 0.01 },
    ratio: { dimension: 'ratio', factor: 1 },
    celsius: { dimension: 'temperature', factor: 1 },
    hertz: { dimension: 'frequency', factor: 1 },
    cores: { dimension: 'cores', factor: 1 },
};

/**
 * The unit systems whose base is each dimension's base, the first being
 * the one a chart uses when its spec names a stored unit and no system.
 */
const DIMENSION_SYSTEMS = {
    time: ['time'],
    bytes: ['bytes', 'datarate'],
    bits: ['bitrate'],
    ratio: ['percentage'],
    temperature: ['temperature'],
    frequency: ['frequency'],
    cores: ['cores'],
};

/**
 * The unit system a chart formats its values with, and the factor a value
 * is multiplied by to be in that system's base, from the chart's `format`.
 *
 * `unit` is the unit of the values the query returns; set it only when the
 * query has not rescaled them. A `unit_system` of another dimension ignores
 * it. Within the unit's dimension the chart rescales into the system's
 * base: a `seconds` value by 1e9 into `time` (nanoseconds), a `kibibytes`
 * value by 1024 into `bytes` or `datarate`, a `percent` value by 0.01 into
 * `percentage` (a fraction).
 *
 * - `format.unit_system` alone: that system, factor 1.
 * - `format.unit` (`STORED_UNITS`) alone: the first system of the unit's
 *   dimension (`DIMENSION_SYSTEMS`), and the unit's factor.
 * - both, where `unit_system` is a system of the unit's dimension: that
 *   system and the unit's factor.
 * - both, where it is not: `unit_system`, factor 1.
 *
 * An unknown `unit` is ignored.
 *
 * @param {object} [format] - the spec's `opts.format`
 * @return {{unitSystem: (string|undefined), factor: number}}
 */
function resolveUnit(format = {}) {
    const unitSystem = format.unit_system || undefined;
    const stored = Object.hasOwn(STORED_UNITS, format.unit ?? '')
        ? STORED_UNITS[format.unit]
        : null;
    if (!stored) return { unitSystem, factor: 1 };
    const systems = DIMENSION_SYSTEMS[stored.dimension];
    if (unitSystem && !systems.includes(unitSystem)) return { unitSystem, factor: 1 };
    return { unitSystem: unitSystem || systems[0], factor: stored.factor };
}

/**
 * `+Inf` or `-Inf` for ±Infinity, as PromQL writes them, and null for any
 * other value. A histogram quantile past the highest finite bucket is +Inf.
 */
function infinityLabel(value) {
    if (value === Infinity) return '+Inf';
    if (value === -Infinity) return '-Inf';
    return null;
}

/**
 * A value shown with no unit system: `+Inf` or `-Inf` for ±Infinity, any
 * other value unchanged.
 */
function plainValue(value) {
    return infinityLabel(value) ?? value;
}

/**
 * Format a value with the appropriate unit scaling
 * 
 * @param {number} value - The value to format
 * @param {string} unitSystem - The unit system to use ('time', 'bytes', 'bitrate', etc.)
 * @param {number} precision - Number of decimal places to include (default: 2)
 * @param {number} factor - What the value is multiplied by to be in the
 *     system's base (default: 1); see `resolveUnit`
 * @return {object} Object with formatted value and unit string
 */
function formatWithUnit(value, unitSystem, precision = 2, factor = 1) {
    if (value === null || value === undefined || isNaN(value)) {
        return { value: '0', unit: UNIT_SYSTEMS[unitSystem]?.base || '' };
    }
    const inf = infinityLabel(value);
    if (inf) return { value: inf, unit: '' };
    value *= factor;

    const absValue = Math.abs(value);

    const system = UNIT_SYSTEMS[unitSystem];
    if (!system) {
        // Fallback for unknown unit systems
        return {
            value: value.toFixed(precision),
            unit: unitSystem || ''
        };
    }

    // Pick the largest scale whose threshold the value reaches.
    let scale = system.scales[0];
    for (let i = system.scales.length - 1; i >= 0; i--) {
        if (absValue >= system.scales[i].threshold) {
            scale = system.scales[i];
            break;
        }
    }

    const multiplier = scale.multiplier || 1; // e.g. percentages scale by 100
    const scaledValue = ((value * multiplier) / scale.divisor).toFixed(precision);

    // Remove trailing zeros after decimal point
    const cleanValue = scaledValue.replace(/\.0+$/, '').replace(/(\.\d*[1-9])0+$/, '$1');

    return {
        value: cleanValue,
        unit: scale.suffix
    };
}

/**
 * Creates a formatter function for ECharts that applies unit scaling
 * 
 * @param {string} unitSystem - The unit system to use
 * @param {number} precision - Number of decimal places (default: 2)
 * @param {number} factor - What a value is multiplied by to be in the
 *     system's base (default: 1); see `resolveUnit`
 * @return {Function} Formatter function for ECharts
 */
function createAxisLabelFormatter(unitSystem, precision = 2, factor = 1) {
    return function (value) {
        if (value === '' || value === null || value === undefined) return '';

        const formatted = formatWithUnit(value, unitSystem, precision, factor);
        return formatted.value + (formatted.unit ? ' ' + formatted.unit : '');
    };
}

export {
    UNIT_SYSTEMS,
    STORED_UNITS,
    resolveUnit,
    infinityLabel,
    plainValue,
    formatWithUnit,
    createAxisLabelFormatter,
};