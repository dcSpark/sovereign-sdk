export interface ServiceHealth {
  name: string;
  url: string;
  status: string;
  error?: string;
  response_time_ms?: number;
}

export interface HealthResponse {
  status: string;
  services: ServiceHealth[];
  checkedAt: string;
}

export type ActionType = 'start' | 'stop' | 'restart' | 'clean';

export interface ActionResult {
  success: boolean;
  message: string;
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
