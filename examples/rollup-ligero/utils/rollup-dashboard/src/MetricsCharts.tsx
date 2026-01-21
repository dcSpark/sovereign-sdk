import { useState, useEffect, useCallback } from 'react';
import {
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Area,
  AreaChart,
} from 'recharts';
import type {
  HistoricMetricsData,
  ChartDataPoint,
  TpsHistoricResponse,
  TotalTransactionsHistoricResponse,
  FailedTransactionsRateHistoricResponse,
  AverageTransactionSizeHistoricResponse,
  MedianTransactionSizeHistoricResponse,
  TokenValueSpentHistoricResponse,
  TokenVelocityHistoricResponse,
} from './types';
import { fetchHistoricMetrics } from './api';

interface MetricsChartsProps {
  autoRefresh: boolean;
}

type TimeRange = '15m' | '1h' | '6h' | '24h';

const TIME_RANGE_MINUTES: Record<TimeRange, number> = {
  '15m': 15,
  '1h': 60,
  '6h': 360,
  '24h': 1440,
};

// Transform functions for each metric type
function transformTpsData(data: TpsHistoricResponse | null): ChartDataPoint[] {
  if (!data?.samples?.length) return [];
  return data.samples
    .filter((s) => s.tps !== null)
    .map((s) => ({
      time: new Date(s.recorded_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
      timestamp: s.recorded_at_ms,
      value: s.tps ?? 0,
    }))
    .sort((a, b) => a.timestamp - b.timestamp);
}

function transformTotalTxData(data: TotalTransactionsHistoricResponse | null): ChartDataPoint[] {
  if (!data?.samples?.length) return [];
  return data.samples
    .map((s) => ({
      time: new Date(s.recorded_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
      timestamp: s.recorded_at_ms,
      value: s.payload?.total_transactions ?? 0,
    }))
    .sort((a, b) => a.timestamp - b.timestamp);
}

function transformFailedRateData(data: FailedTransactionsRateHistoricResponse | null): ChartDataPoint[] {
  if (!data?.samples?.length) return [];
  return data.samples
    .filter((s) => s.rate_percent !== null)
    .map((s) => ({
      time: new Date(s.recorded_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
      timestamp: s.recorded_at_ms,
      value: s.rate_percent ?? 0,
    }))
    .sort((a, b) => a.timestamp - b.timestamp);
}

function transformAvgSizeData(data: AverageTransactionSizeHistoricResponse | null): ChartDataPoint[] {
  if (!data?.samples?.length) return [];
  return data.samples
    .filter((s) => s.average_amount !== null)
    .map((s) => ({
      time: new Date(s.recorded_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
      timestamp: s.recorded_at_ms,
      value: s.average_amount ?? 0,
    }))
    .sort((a, b) => a.timestamp - b.timestamp);
}

function transformMedianSizeData(data: MedianTransactionSizeHistoricResponse | null): ChartDataPoint[] {
  if (!data?.samples?.length) return [];
  return data.samples
    .filter((s) => s.median_amount !== null)
    .map((s) => ({
      time: new Date(s.recorded_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
      timestamp: s.recorded_at_ms,
      value: s.median_amount ?? 0,
    }))
    .sort((a, b) => a.timestamp - b.timestamp);
}

function transformValueSpentData(data: TokenValueSpentHistoricResponse | null): ChartDataPoint[] {
  if (!data?.samples?.length) return [];
  return data.samples
    .filter((s) => s.value_spent !== null)
    .map((s) => ({
      time: new Date(s.recorded_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
      timestamp: s.recorded_at_ms,
      value: s.value_spent ?? 0,
    }))
    .sort((a, b) => a.timestamp - b.timestamp);
}

function transformVelocityData(data: TokenVelocityHistoricResponse | null): ChartDataPoint[] {
  if (!data?.samples?.length) return [];
  return data.samples
    .filter((s) => s.token_velocity !== null)
    .map((s) => ({
      time: new Date(s.recorded_at_ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
      timestamp: s.recorded_at_ms,
      value: s.token_velocity ?? 0,
    }))
    .sort((a, b) => a.timestamp - b.timestamp);
}

function formatYAxisValue(value: number, metric: string): string {
  if (metric === 'tps') {
    if (value >= 100) return value.toFixed(0);
    if (value >= 10) return value.toFixed(1);
    return value.toFixed(2);
  }
  if (metric === 'failedRate') {
    return `${value.toFixed(1)}%`;
  }
  if (metric === 'velocity') {
    // Token velocity is a small ratio, show more precision
    if (value >= 1) return value.toFixed(2);
    if (value >= 0.01) return value.toFixed(3);
    return value.toFixed(4);
  }
  // Token metrics - values are already in human-readable format (not raw)
  if (metric === 'tokenValue' || metric === 'avgSize' || metric === 'medianSize') {
    if (Math.abs(value) >= 1_000_000) {
      return `${(value / 1_000_000).toFixed(1)}M`;
    }
    if (Math.abs(value) >= 1_000) {
      return `${(value / 1_000).toFixed(1)}K`;
    }
    if (Math.abs(value) >= 1) {
      return value.toFixed(1);
    }
    if (Math.abs(value) >= 0.01) {
      return value.toFixed(2);
    }
    return value.toFixed(4);
  }
  // Default: totalTx and others
  if (Math.abs(value) >= 1_000_000) {
    return `${(value / 1_000_000).toFixed(1)}M`;
  }
  if (Math.abs(value) >= 1_000) {
    return `${(value / 1_000).toFixed(1)}K`;
  }
  return value.toFixed(0);
}

function formatTooltipValue(value: number, metric: string): string {
  if (metric === 'tps') {
    return `${value.toFixed(4)} tx/s`;
  }
  if (metric === 'failedRate') {
    return `${value.toFixed(2)}%`;
  }
  if (metric === 'velocity') {
    return `${value.toFixed(6)} (turnover)`;
  }
  // Token metrics - values are already in human-readable format
  if (metric === 'tokenValue' || metric === 'avgSize' || metric === 'medianSize') {
    if (Math.abs(value) >= 1000) {
      return `${value.toLocaleString(undefined, { maximumFractionDigits: 2 })} tokens`;
    }
    return `${value.toLocaleString(undefined, { maximumFractionDigits: 4 })} tokens`;
  }
  return value.toLocaleString();
}

interface ChartCardProps {
  title: string;
  data: ChartDataPoint[];
  color: string;
  metric: string;
  loading?: boolean;
}

function calculateYAxisWidth(metric: string): number {
  // Token metrics need more width for larger formatted numbers
  if (metric === 'tokenValue' || metric === 'avgSize' || metric === 'medianSize') {
    return 65;
  }
  if (metric === 'totalTx') {
    return 55;
  }
  return 50;
}

function ChartCard({ title, data, color, metric, loading }: ChartCardProps) {
  const hasData = data.length > 0;

  // Calculate domain with some padding
  const values = data.map((d) => d.value).filter((v) => v !== null && isFinite(v));
  const minValue = values.length > 0 ? Math.min(...values) : 0;
  const maxValue = values.length > 0 ? Math.max(...values) : 1;
  const range = maxValue - minValue;
  const padding = range * 0.1 || maxValue * 0.1 || 1;
  const yDomain: [number, number] = [
    Math.max(0, minValue - padding),
    maxValue + padding,
  ];

  return (
    <div className="chart-card">
      <div className="chart-header">
        <h3>{title}</h3>
      </div>
      <div className="chart-container">
        {loading ? (
          <div className="chart-loading">Loading...</div>
        ) : !hasData ? (
          <div className="chart-no-data">No data available</div>
        ) : (
          <ResponsiveContainer width="100%" height={200}>
            <AreaChart data={data} margin={{ top: 10, right: 10, left: 0, bottom: 0 }}>
              <defs>
                <linearGradient id={`gradient-${metric}`} x1="0" y1="0" x2="0" y2="1">
                  <stop offset="5%" stopColor={color} stopOpacity={0.3} />
                  <stop offset="95%" stopColor={color} stopOpacity={0} />
                </linearGradient>
              </defs>
              <CartesianGrid strokeDasharray="3 3" stroke="#222" />
              <XAxis
                dataKey="time"
                stroke="#666"
                fontSize={10}
                tickLine={false}
                axisLine={false}
              />
              <YAxis
                stroke="#666"
                fontSize={10}
                tickLine={false}
                axisLine={false}
                tickFormatter={(value) => formatYAxisValue(value, metric)}
                width={calculateYAxisWidth(metric)}
                domain={yDomain}
                tickCount={5}
                allowDataOverflow={false}
              />
              <Tooltip
                contentStyle={{
                  backgroundColor: '#1a1a1a',
                  border: '1px solid #333',
                  borderRadius: '4px',
                  fontSize: '12px',
                }}
                labelStyle={{ color: '#888' }}
                formatter={(value: number) => [formatTooltipValue(value, metric), title]}
              />
              <Area
                type="monotone"
                dataKey="value"
                stroke={color}
                strokeWidth={2}
                fill={`url(#gradient-${metric})`}
              />
            </AreaChart>
          </ResponsiveContainer>
        )}
      </div>
    </div>
  );
}

export function MetricsCharts({ autoRefresh }: MetricsChartsProps) {
  const [historicData, setHistoricData] = useState<HistoricMetricsData | null>(null);
  const [timeRange, setTimeRange] = useState<TimeRange>('1h');
  const [loading, setLoading] = useState(true);

  const loadHistoricData = useCallback(async () => {
    setLoading(true);
    const data = await fetchHistoricMetrics(TIME_RANGE_MINUTES[timeRange]);
    setHistoricData(data);
    setLoading(false);
  }, [timeRange]);

  useEffect(() => {
    loadHistoricData();
    if (autoRefresh) {
      // Refresh historic data every 30 seconds
      const interval = setInterval(loadHistoricData, 30000);
      return () => clearInterval(interval);
    }
  }, [loadHistoricData, autoRefresh]);

  const tpsData = transformTpsData(historicData?.tps ?? null);
  const totalTxData = transformTotalTxData(historicData?.totalTransactions ?? null);
  const failedRateData = transformFailedRateData(historicData?.failedTransactionsRate ?? null);
  const avgSizeData = transformAvgSizeData(historicData?.averageTransactionSize ?? null);
  const medianSizeData = transformMedianSizeData(historicData?.medianTransactionSize ?? null);
  const valueSpentData = transformValueSpentData(historicData?.tokenValueSpent ?? null);
  const velocityData = transformVelocityData(historicData?.tokenVelocity ?? null);

  return (
    <section className="charts-section">
      <div className="charts-header">
        <h2>Historic Metrics</h2>
        <div className="time-range-selector">
          {(['15m', '1h', '6h', '24h'] as TimeRange[]).map((range) => (
            <button
              key={range}
              className={`time-range-btn ${timeRange === range ? 'active' : ''}`}
              onClick={() => setTimeRange(range)}
            >
              {range}
            </button>
          ))}
        </div>
      </div>

      {historicData?.error ? (
        <div className="charts-error">
          <span>{historicData.error}</span>
        </div>
      ) : (
        <div className="charts-grid">
          <ChartCard
            title="TPS"
            data={tpsData}
            color="#ff3333"
            metric="tps"
            loading={loading}
          />
          <ChartCard
            title="Total Transactions"
            data={totalTxData}
            color="#00ff88"
            metric="totalTx"
            loading={loading}
          />
          <ChartCard
            title="Failed Rate (%)"
            data={failedRateData}
            color="#ffaa00"
            metric="failedRate"
            loading={loading}
          />
          <ChartCard
            title="Avg Transaction Size"
            data={avgSizeData}
            color="#8855ff"
            metric="avgSize"
            loading={loading}
          />
          <ChartCard
            title="Median Transaction Size"
            data={medianSizeData}
            color="#00aaff"
            metric="medianSize"
            loading={loading}
          />
          <ChartCard
            title="Token Value Spent"
            data={valueSpentData}
            color="#ff55aa"
            metric="tokenValue"
            loading={loading}
          />
          <ChartCard
            title="Token Velocity"
            data={velocityData}
            color="#55ffaa"
            metric="velocity"
            loading={loading}
          />
        </div>
      )}
    </section>
  );
}
