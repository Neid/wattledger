-- RCE prices from PSE, one row per 15-minute slot.
create table rce_price (
    slot_start    timestamptz primary key,
    slot_end      timestamptz not null check (slot_end > slot_start),
    business_date date not null,
    price_pln_mwh double precision not null,
    fetched_at    timestamptz not null default now()
);
create index rce_price_business_date on rce_price (business_date);

-- User settings: exactly one row.
create table settings (
    id                   boolean primary key default true check (id),
    battery_capacity_kwh double precision not null default 16.1
        check (battery_capacity_kwh > 0 and battery_capacity_kwh <= 100),
    -- Written to the inverter's max sell power and time-of-use slot power
    -- during an export.
    max_export_power_w   integer not null default 4000
        check (max_export_power_w between 100 and 10000),
    -- Lowest stop SOC a plan may use.
    min_soc              integer not null default 10 check (min_soc between 5 and 99),
    -- Home consumption the battery also covers while exporting (projection).
    home_load_w          integer not null default 400 check (home_load_w between 0 and 20000),
    discharge_efficiency double precision not null default 0.95
        check (discharge_efficiency > 0 and discharge_efficiency <= 1),
    -- Multiplies the RCE value of exported energy (1.0 = plain RCE).
    export_price_factor  double precision not null default 1.0
        check (export_price_factor > 0 and export_price_factor <= 5),
    updated_at           timestamptz not null default now()
);
insert into settings default values;

create table export_plan (
    id               bigserial primary key,
    created_at       timestamptz not null default now(),
    start_at         timestamptz not null,
    -- Latest end: the export stops then even if the SOC was not reached.
    end_at           timestamptz not null check (end_at > start_at),
    target_soc       integer not null check (target_soc between 1 and 99),
    power_w          integer not null check (power_w > 0),
    status           text not null default 'scheduled'
        check (status in ('scheduled', 'active', 'done', 'cancelled', 'failed', 'missed')),
    cancel_requested boolean not null default false,
    retry_after      timestamptz,
    started_at       timestamptz,
    finished_at      timestamptz,
    -- Why it ended, or the last error.
    note             text,
    -- The bridge answered in dry-run mode: nothing was written.
    dry_run          boolean,
    -- Inverter settings read before the export; written back at the end.
    saved_settings   jsonb,
    soc_at_start     double precision,
    projected_kwh    double precision,
    projected_pln    double precision
);
create index export_plan_open on export_plan (start_at) where status in ('scheduled', 'active');

-- Every command sent to the bridge, with its answer.
create table inverter_command (
    id      text primary key,
    plan_id bigint references export_plan (id),
    purpose text not null,
    sent_at timestamptz not null default now(),
    request jsonb not null,
    result  jsonb,
    ok      boolean
);
create index inverter_command_plan on inverter_command (plan_id);
