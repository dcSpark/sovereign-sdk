import { useState, useEffect, useCallback } from 'react';
import type { HealthResponse, ActionType, ActionResult, MetricsData } from './types';
import { fetchHealth, performAction, fetchMetrics } from './api';
import { MetricsCharts } from './MetricsCharts';
import './styles.css';

function App() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [metrics, setMetrics] = useState<MetricsData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [actionLoading, setActionLoading] = useState<ActionType | null>(null);
  const [actionResult, setActionResult] = useState<ActionResult | null>(null);
  const [autoRefresh, setAutoRefresh] = useState(true);

  const loadHealth = useCallback(async () => {
    try {
      const data = await fetchHealth();
      setHealth(data);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch health');
    } finally {
      setLoading(false);
    }
  }, []);

  const loadMetrics = useCallback(async () => {
    const data = await fetchMetrics();
    setMetrics(data);
  }, []);

  useEffect(() => {
    loadHealth();
    loadMetrics();
    if (autoRefresh) {
      const interval = setInterval(() => {
        loadHealth();
        loadMetrics();
      }, 5000);
      return () => clearInterval(interval);
    }
  }, [loadHealth, loadMetrics, autoRefresh]);

  const handleAction = async (action: ActionType) => {
    setActionLoading(action);
    setActionResult(null);
    const result = await performAction(action);
    setActionResult(result);
    setActionLoading(null);
    setTimeout(loadHealth, 1000);
  };

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'healthy':
        return 'status-healthy';
      case 'unhealthy':
        return 'status-unhealthy';
      default:
        return 'status-unknown';
    }
  };

  const formatResponseTime = (ms?: number) => {
    if (ms === undefined) return '-';
    return `${ms.toFixed(0)}ms`;
  };

  const formatServiceNumber = (index: number) => {
    return String(index + 1).padStart(2, '0');
  };

  const formatNumber = (value: number | undefined | null, decimals: number = 2): string => {
    if (value === undefined || value === null) return '-';
    if (Math.abs(value) >= 1_000_000) {
      return `${(value / 1_000_000).toFixed(decimals)}M`;
    }
    if (Math.abs(value) >= 1_000) {
      return `${(value / 1_000).toFixed(decimals)}K`;
    }
    return value.toFixed(decimals);
  };

  const formatPercent = (value: number | undefined | null): string => {
    if (value === undefined || value === null) return '-';
    return `${value.toFixed(2)}%`;
  };

  const formatTps = (value: number | undefined | null): string => {
    if (value === undefined || value === null) return '-';
    return value.toFixed(3);
  };

  const formatTokenAmount = (value: number | undefined | null): string => {
    if (value === undefined || value === null) return '-';
    // Values from API are already in human-readable format (not raw)
    if (Math.abs(value) >= 1_000_000) {
      return `${(value / 1_000_000).toFixed(2)}M`;
    }
    if (Math.abs(value) >= 1_000) {
      return `${(value / 1_000).toFixed(2)}K`;
    }
    if (Math.abs(value) >= 1) {
      return value.toFixed(2);
    }
    return value.toFixed(4);
  };

  return (
    <div className="app">
      <header className="header">
        <h1>Midnight L2 Service Dashboard</h1>
        <div className="header-controls">
          <label className="auto-refresh">
            <input
              type="checkbox"
              checked={autoRefresh}
              onChange={(e) => setAutoRefresh(e.target.checked)}
            />
            Auto-refresh
          </label>
          <button className="refresh-btn" onClick={loadHealth} disabled={loading}>
            {loading ? 'Loading...' : 'Refresh'}
          </button>
        </div>
      </header>

      <main className="main">
        {/* Action Controls */}
        <section className="controls-section">
          <h2>Service Controls</h2>
          <div className="controls">
            <button
              className="control-btn start"
              onClick={() => handleAction('start')}
              disabled={actionLoading !== null}
            >
              {actionLoading === 'start' ? 'Starting...' : 'Start'}
            </button>
            <button
              className="control-btn stop"
              onClick={() => handleAction('stop')}
              disabled={actionLoading !== null}
            >
              {actionLoading === 'stop' ? 'Stopping...' : 'Stop'}
            </button>
            <button
              className="control-btn restart"
              onClick={() => handleAction('restart')}
              disabled={actionLoading !== null}
            >
              {actionLoading === 'restart' ? 'Restarting...' : 'Restart'}
            </button>
            <button
              className="control-btn clean"
              onClick={() => handleAction('clean')}
              disabled={actionLoading !== null}
            >
              {actionLoading === 'clean' ? 'Cleaning...' : 'Clean'}
            </button>
          </div>
          {actionResult && (
            <div className={`action-result ${actionResult.success ? 'success' : 'error'}`}>
              {actionResult.message}
            </div>
          )}
        </section>

        {/* Overall Status */}
        {error ? (
          <div className="error-banner">
            <strong>Connection Error</strong>
            <p className="error-hint">
              Service controller unreachable at <code>http://127.0.0.1:9090</code>
            </p>
          </div>
        ) : health ? (
          <>
            <section className="status-section">
              <div className={`overall-status ${getStatusColor(health.status)}`}>
                <span className="status-indicator"></span>
                <span className="status-text">
                  System {health.status}
                </span>
                <span className="checked-at">
                  {new Date(health.checkedAt).toLocaleTimeString()}
                </span>
              </div>
            </section>

            {/* Services Grid */}
            <section className="services-section">
              <h2>Services</h2>
              <div className="services-grid">
                {health.services.map((service, index) => (
                  <div
                    key={service.name}
                    className={`service-card ${getStatusColor(service.status)}`}
                  >
                    <div className="service-header">
                      <span className="service-number">{formatServiceNumber(index)}</span>
                      <span className="status-dot"></span>
                      <h3>{service.name}</h3>
                    </div>
                    <div className="service-details">
                      <div className="detail-row">
                        <span className="label">Endpoint</span>
                        <span className="value url">{service.url}</span>
                      </div>
                      <div className="detail-row">
                        <span className="label">Status</span>
                        <span className={`value ${service.status}`}>
                          {service.status}
                        </span>
                      </div>
                      <div className="detail-row">
                        <span className="label">Latency</span>
                        <span className="value">
                          {formatResponseTime(service.response_time_ms)}
                        </span>
                      </div>
                      {service.error && (
                        <div className="detail-row error">
                          <span className="label">Error</span>
                          <span className="value">{service.error}</span>
                        </div>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            </section>

            {/* Metrics Section */}
            <section className="metrics-section">
              <h2>Network Metrics</h2>
              {metrics?.error ? (
                <div className="metrics-error">
                  <span>Metrics unavailable: {metrics.error}</span>
                </div>
              ) : (
                <div className="metrics-grid">
                  {/* TPS Card */}
                  <div className="metric-card highlight">
                    <div className="metric-header">
                      <span className="metric-icon">⚡</span>
                      <h3>TPS</h3>
                    </div>
                    <div className="metric-value-large">
                      {formatTps(metrics?.tps?.tps)}
                    </div>
                    <div className="metric-subtitle">transactions per second</div>
                    {metrics?.tps && (
                      <div className="metric-details">
                        <div className="metric-detail">
                          <span className="label">Delta</span>
                          <span className="value">{formatNumber(metrics.tps.delta_transactions, 0)} tx</span>
                        </div>
                      </div>
                    )}
                  </div>

                  {/* Total Transactions Card */}
                  <div className="metric-card">
                    <div className="metric-header">
                      <span className="metric-icon">📊</span>
                      <h3>Total Transactions</h3>
                    </div>
                    <div className="metric-value-large">
                      {formatNumber(metrics?.totalTransactions?.total_transactions, 0)}
                    </div>
                    <div className="metric-subtitle">cumulative</div>
                  </div>

                  {/* Failed Transactions Rate Card */}
                  <div className="metric-card">
                    <div className="metric-header">
                      <span className="metric-icon">⚠️</span>
                      <h3>Failed Rate</h3>
                    </div>
                    <div className={`metric-value-large ${(metrics?.failedTransactionsRate?.rate_percent ?? 0) > 5 ? 'warning' : ''}`}>
                      {formatPercent(metrics?.failedTransactionsRate?.rate_percent)}
                    </div>
                    <div className="metric-subtitle">failure rate</div>
                    {metrics?.failedTransactionsRate && (
                      <div className="metric-details">
                        <div className="metric-detail">
                          <span className="label">Failed</span>
                          <span className="value">{formatNumber(metrics.failedTransactionsRate.failed_transactions, 0)}</span>
                        </div>
                        <div className="metric-detail">
                          <span className="label">Total</span>
                          <span className="value">{formatNumber(metrics.failedTransactionsRate.total_transactions, 0)}</span>
                        </div>
                      </div>
                    )}
                  </div>

                  {/* Average Transaction Size Card */}
                  <div className="metric-card">
                    <div className="metric-header">
                      <span className="metric-icon">📏</span>
                      <h3>Avg Tx Size</h3>
                    </div>
                    <div className="metric-value-large">
                      {formatTokenAmount(metrics?.averageTransactionSize?.average_amount)}
                    </div>
                    <div className="metric-subtitle">tokens (24h avg)</div>
                  </div>

                  {/* Median Transaction Size Card */}
                  <div className="metric-card">
                    <div className="metric-header">
                      <span className="metric-icon">📐</span>
                      <h3>Median Tx Size</h3>
                    </div>
                    <div className="metric-value-large">
                      {formatTokenAmount(metrics?.medianTransactionSize?.median_amount)}
                    </div>
                    <div className="metric-subtitle">tokens (24h median)</div>
                  </div>

                  {/* Token Value Spent Card */}
                  <div className="metric-card">
                    <div className="metric-header">
                      <span className="metric-icon">💰</span>
                      <h3>Value Spent</h3>
                    </div>
                    <div className="metric-value-large">
                      {formatTokenAmount(metrics?.tokenValueSpent?.value_spent)}
                    </div>
                    <div className="metric-subtitle">tokens (24h)</div>
                  </div>

                  {/* Token Velocity Card */}
                  <div className="metric-card">
                    <div className="metric-header">
                      <span className="metric-icon">🔄</span>
                      <h3>Token Velocity</h3>
                    </div>
                    <div className="metric-value-large">
                      {formatNumber(metrics?.tokenVelocity?.token_velocity, 4)}
                    </div>
                    <div className="metric-subtitle">turnover rate (24h)</div>
                    {metrics?.tokenVelocity && (
                      <div className="metric-details">
                        <div className="metric-detail">
                          <span className="label">Supply</span>
                          <span className="value">{formatTokenAmount(metrics.tokenVelocity.total_tokens)}</span>
                        </div>
                      </div>
                    )}
                  </div>
                </div>
              )}
            </section>

            {/* Historic Metrics Charts */}
            <MetricsCharts autoRefresh={autoRefresh} />
          </>
        ) : (
          <div className="loading">Loading services...</div>
        )}
      </main>

      <footer className="footer">
        <p>
          Service Controller <code>http://127.0.0.1:9090</code>
        </p>
      </footer>
    </div>
  );
}

export default App;
