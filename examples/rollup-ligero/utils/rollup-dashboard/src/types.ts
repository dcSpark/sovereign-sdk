export interface ServiceHealth {
  id: string;
  name: string;
  url: string;
  status: string;
  remote?: boolean;
  controllable?: boolean;
  running: boolean;
  pid?: number;
  error?: string;
  response_time_ms?: number;
}

export interface HealthResponse {
  status: string;
  services: ServiceHealth[];
  checkedAt: string;
}

export type ActionType = 'start' | 'stop' | 'restart' | 'clean' | 'clean-database' | 'reset-tee';

export interface ActionResult {
  success: boolean;
  message: string;
}

export type EmaWindow = 's2' | 's5' | 'm1' | 'm5' | 'm15';

export interface EmaMetricsResponse {
  Accounts: number;
  SendingAccounts: number;
  TPS: number;
  PeakTPS: number;
  PeakTPSAtMs: number | null;
  TokensPerSecond: number;
  TotalDisclosureEvents: number;
  TotalTokensInWallets: number;
  TotalTransactions: number;
}

// Metrics API types
export interface TpsResponse {
  tps: number;
  delta_transactions: number;
  delta_ms: number;
  latest_total: number;
}

export interface TotalTransactionsResponse {
  total_transactions: number;
  as_of_ms: number;
}

export interface FailedTransactionsRateResponse {
  rate_percent: number;
  failed_transactions: number;
  total_transactions: number;
  delta_ms: number;
  retention_seconds: number;
}

export interface AverageTransactionSizeResponse {
  average_amount: number;
  delta_amount: number;
  delta_transactions: number;
  delta_ms: number;
  retention_seconds: number;
}

export interface MedianTransactionSizeResponse {
  median_amount: number;
}

export interface TokenValueSpentResponse {
  value_spent: number;
}

export interface TokenVelocityResponse {
  token_velocity: number;
  value_spent: number;
  total_tokens: number;
}

export interface MetricsData {
  tps: TpsResponse | null;
  totalTransactions: TotalTransactionsResponse | null;
  failedTransactionsRate: FailedTransactionsRateResponse | null;
  averageTransactionSize: AverageTransactionSizeResponse | null;
  medianTransactionSize: MedianTransactionSizeResponse | null;
  tokenValueSpent: TokenValueSpentResponse | null;
  tokenVelocity: TokenVelocityResponse | null;
  error?: string;
}

// Historic endpoint types - samples have different structures per metric
export interface TpsSample {
  recorded_at_ms: number;
  tps: number | null;
  delta_transactions?: number | null;
  delta_ms?: number | null;
  latest_total?: number | null;
}

export interface TotalTransactionsSample {
  recorded_at_ms: number;
  payload: {
    total_transactions: number;
  };
}

export interface FailedTransactionsRateSample {
  recorded_at_ms: number;
  rate_percent: number | null;
  failed_transactions?: number | null;
  total_transactions?: number | null;
  delta_ms?: number | null;
}

export interface AverageTransactionSizeSample {
  recorded_at_ms: number;
  average_amount: number | null;
  delta_amount?: string | null;
  delta_transactions?: number | null;
  delta_ms?: number | null;
}

export interface MedianTransactionSizeSample {
  recorded_at_ms: number;
  median_amount: number | null;
}

export interface TokenValueSpentSample {
  recorded_at_ms: number;
  value_spent: number | null;
}

export interface TokenVelocitySample {
  recorded_at_ms: number;
  token_velocity: number | null;
  value_spent?: number | null;
  total_tokens?: number | null;
}

export interface TpsHistoricResponse {
  name: string;
  interval_secs: number;
  latest: TpsSample | null;
  samples: TpsSample[];
}

export interface TotalTransactionsHistoricResponse {
  name: string;
  interval_secs: number;
  latest: TotalTransactionsSample | null;
  samples: TotalTransactionsSample[];
}

export interface FailedTransactionsRateHistoricResponse {
  name: string;
  interval_secs: number;
  latest: FailedTransactionsRateSample | null;
  samples: FailedTransactionsRateSample[];
}

export interface AverageTransactionSizeHistoricResponse {
  name: string;
  interval_secs: number;
  latest: AverageTransactionSizeSample | null;
  samples: AverageTransactionSizeSample[];
}

export interface MedianTransactionSizeHistoricResponse {
  name: string;
  bucket_seconds: number;
  latest: MedianTransactionSizeSample | null;
  samples: MedianTransactionSizeSample[];
}

export interface TokenValueSpentHistoricResponse {
  name: string;
  interval_secs: number;
  latest: TokenValueSpentSample | null;
  samples: TokenValueSpentSample[];
}

export interface TokenVelocityHistoricResponse {
  name: string;
  interval_secs: number;
  latest: TokenVelocitySample | null;
  samples: TokenVelocitySample[];
}

export interface HistoricMetricsData {
  tps: TpsHistoricResponse | null;
  totalTransactions: TotalTransactionsHistoricResponse | null;
  failedTransactionsRate: FailedTransactionsRateHistoricResponse | null;
  averageTransactionSize: AverageTransactionSizeHistoricResponse | null;
  medianTransactionSize: MedianTransactionSizeHistoricResponse | null;
  tokenValueSpent: TokenValueSpentHistoricResponse | null;
  tokenVelocity: TokenVelocityHistoricResponse | null;
  error?: string;
}

// Chart display data point
export interface ChartDataPoint {
  time: string;
  timestamp: number;
  value: number;
}

// System stats types
export interface CpuStats {
  usage_percent: number;
  core_count: number;
  per_core_usage: number[];
}

export interface MemoryStats {
  total_bytes: number;
  used_bytes: number;
  free_bytes: number;
  available_bytes: number;
  usage_percent: number;
  swap_total_bytes: number;
  swap_used_bytes: number;
}

export interface DiskStats {
  name: string;
  mount_point: string;
  total_bytes: number;
  available_bytes: number;
  used_bytes: number;
  usage_percent: number;
}

export interface LoadAverage {
  one: number;
  five: number;
  fifteen: number;
}

export interface SystemStats {
  cpu: CpuStats;
  memory: MemoryStats;
  disks: DiskStats[];
  uptime_seconds: number;
  load_average: LoadAverage;
}
