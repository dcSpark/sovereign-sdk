import type {
  HealthResponse,
  ActionType,
  ActionResult,
  MetricsData,
  TpsResponse,
  TotalTransactionsResponse,
  FailedTransactionsRateResponse,
  AverageTransactionSizeResponse,
  MedianTransactionSizeResponse,
  TokenValueSpentResponse,
  TokenVelocityResponse,
  HistoricMetricsData,
  TpsHistoricResponse,
  TotalTransactionsHistoricResponse,
  FailedTransactionsRateHistoricResponse,
  AverageTransactionSizeHistoricResponse,
  MedianTransactionSizeHistoricResponse,
  TokenValueSpentHistoricResponse,
  TokenVelocityHistoricResponse,
  SystemStats,
} from './types';

const API_BASE = '/controller';
const METRICS_BASE = '/metrics';

export async function fetchHealth(): Promise<HealthResponse> {
  const response = await fetch(`${API_BASE}/health`);
  if (!response.ok) {
    throw new Error(`Health check failed: ${response.statusText}`);
  }
  return response.json();
}

export async function fetchSystemStats(): Promise<SystemStats | null> {
  try {
    const response = await fetch(`${API_BASE}/stats`);
    if (!response.ok) {
      return null;
    }
    const contentType = response.headers.get('content-type');
    if (!contentType || !contentType.includes('application/json')) {
      return null;
    }
    return response.json();
  } catch {
    return null;
  }
}

export async function performAction(action: ActionType): Promise<ActionResult> {
  try {
    const response = await fetch(`${API_BASE}/${action}`, {
      method: 'POST',
    });
    const message = await response.text();
    return {
      success: response.ok,
      message: message || (response.ok ? 'Success' : 'Failed'),
    };
  } catch (error) {
    return {
      success: false,
      message: error instanceof Error ? error.message : 'Unknown error',
    };
  }
}

// Metrics API functions
async function fetchMetricEndpoint<T>(endpoint: string): Promise<T | null> {
  try {
    const response = await fetch(`${METRICS_BASE}${endpoint}`);
    if (!response.ok) {
      return null;
    }
    // Check if response is JSON before parsing
    const contentType = response.headers.get('content-type');
    if (!contentType || !contentType.includes('application/json')) {
      console.warn(`Metrics endpoint ${endpoint} returned non-JSON response`);
      return null;
    }
    return response.json();
  } catch {
    return null;
  }
}

export async function fetchMetrics(): Promise<MetricsData> {
  try {
    const [
      tps,
      totalTransactions,
      failedTransactionsRate,
      averageTransactionSize,
      medianTransactionSize,
      tokenValueSpent,
      tokenVelocity,
    ] = await Promise.all([
      fetchMetricEndpoint<TpsResponse>('/tps'),
      fetchMetricEndpoint<TotalTransactionsResponse>('/total-transactions'),
      fetchMetricEndpoint<FailedTransactionsRateResponse>('/failed-transactions-rate'),
      fetchMetricEndpoint<AverageTransactionSizeResponse>('/average-transaction-size'),
      fetchMetricEndpoint<MedianTransactionSizeResponse>('/median-transaction-size'),
      fetchMetricEndpoint<TokenValueSpentResponse>('/token-value-spent'),
      fetchMetricEndpoint<TokenVelocityResponse>('/token-velocity'),
    ]);

    // Check if all metrics are null (service likely unavailable)
    const allNull = [tps, totalTransactions, failedTransactionsRate, averageTransactionSize, 
                     medianTransactionSize, tokenValueSpent, tokenVelocity].every(m => m === null);

    return {
      tps,
      totalTransactions,
      failedTransactionsRate,
      averageTransactionSize,
      medianTransactionSize,
      tokenValueSpent,
      tokenVelocity,
      error: allNull ? 'Metrics service unavailable' : undefined,
    };
  } catch (error) {
    return {
      tps: null,
      totalTransactions: null,
      failedTransactionsRate: null,
      averageTransactionSize: null,
      medianTransactionSize: null,
      tokenValueSpent: null,
      tokenVelocity: null,
      error: error instanceof Error ? error.message : 'Failed to fetch metrics',
    };
  }
}

// Historic metrics API functions
export async function fetchHistoricMetrics(windowMinutes: number = 60): Promise<HistoricMetricsData> {
  const now = Date.now();
  const fromMs = now - windowMinutes * 60 * 1000;
  const queryParams = `?from_ms=${fromMs}&to_ms=${now}`;

  try {
    const [
      tps,
      totalTransactions,
      failedTransactionsRate,
      averageTransactionSize,
      medianTransactionSize,
      tokenValueSpent,
      tokenVelocity,
    ] = await Promise.all([
      fetchMetricEndpoint<TpsHistoricResponse>(`/tps/historic${queryParams}`),
      fetchMetricEndpoint<TotalTransactionsHistoricResponse>(`/total-transactions/historic${queryParams}`),
      fetchMetricEndpoint<FailedTransactionsRateHistoricResponse>(`/failed-transactions-rate/historic${queryParams}`),
      fetchMetricEndpoint<AverageTransactionSizeHistoricResponse>(`/average-transaction-size/historic${queryParams}`),
      fetchMetricEndpoint<MedianTransactionSizeHistoricResponse>(`/median-transaction-size/historic${queryParams}`),
      fetchMetricEndpoint<TokenValueSpentHistoricResponse>(`/token-value-spent/historic${queryParams}`),
      fetchMetricEndpoint<TokenVelocityHistoricResponse>(`/token-velocity/historic${queryParams}`),
    ]);

    const allNull = [tps, totalTransactions, failedTransactionsRate, averageTransactionSize,
                     medianTransactionSize, tokenValueSpent, tokenVelocity].every(m => m === null);

    return {
      tps,
      totalTransactions,
      failedTransactionsRate,
      averageTransactionSize,
      medianTransactionSize,
      tokenValueSpent,
      tokenVelocity,
      error: allNull ? 'Historic metrics unavailable' : undefined,
    };
  } catch (error) {
    return {
      tps: null,
      totalTransactions: null,
      failedTransactionsRate: null,
      averageTransactionSize: null,
      medianTransactionSize: null,
      tokenValueSpent: null,
      tokenVelocity: null,
      error: error instanceof Error ? error.message : 'Failed to fetch historic metrics',
    };
  }
}
