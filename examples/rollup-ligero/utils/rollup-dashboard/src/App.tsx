import { useState, useEffect, useCallback } from 'react';
import type { HealthResponse, ActionType, ActionResult, MetricsData } from './types';
import { fetchHealth, performAction, fetchMetrics } from './api';
import { MetricsCharts } from './MetricsCharts';
import { SystemStatsPanel } from './SystemStats';
import { Terminal } from './Terminal';
import './styles.css';

interface RowProps {
  title: string;
  children: React.ReactNode;
  defaultExpanded?: boolean;
  badge?: string;
  badgeColor?: 'healthy' | 'unhealthy' | 'warning' | 'neutral';
}

function DashboardRow({ title, children, defaultExpanded = true, badge, badgeColor = 'neutral' }: RowProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);
  
  return (
    <div className={`dashboard-row ${expanded ? 'expanded' : 'collapsed'}`}>
      <button className="row-header" onClick={() => setExpanded(!expanded)}>
        <span className="row-chevron">{expanded ? '▼' : '▶'}</span>
        <span className="row-title">{title}</span>
        {badge && (
          <span className={`row-badge badge-${badgeColor}`}>{badge}</span>
        )}
      </button>
      {expanded && <div className="row-content">{children}</div>}
    </div>
  );
}

function App() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [metrics, setMetrics] = useState<MetricsData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [actionLoading, setActionLoading] = useState<ActionType | null>(null);
  const [actionResult, setActionResult] = useState<ActionResult | null>(null);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [refreshInterval, setRefreshInterval] = useState(5000);

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
      }, refreshInterval);
      return () => clearInterval(interval);
    }
  }, [loadHealth, loadMetrics, autoRefresh, refreshInterval]);

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

  const formatNumber = (value: number | undefined | null, decimals: number = 2): string => {
    if (value === undefined || value === null || typeof value !== 'number' || !isFinite(value)) return '-';
    if (Math.abs(value) >= 1_000_000) {
      return `${(value / 1_000_000).toFixed(decimals)}M`;
    }
    if (Math.abs(value) >= 1_000) {
      return `${(value / 1_000).toFixed(decimals)}K`;
    }
    return value.toFixed(decimals);
  };

  const formatPercent = (value: number | undefined | null): string => {
    if (value === undefined || value === null || typeof value !== 'number' || !isFinite(value)) return '-';
    return `${value.toFixed(2)}%`;
  };

  const formatTps = (value: number | undefined | null): string => {
    if (value === undefined || value === null || typeof value !== 'number' || !isFinite(value)) return '-';
    return value.toFixed(3);
  };

  const formatTokenAmount = (value: number | undefined | null): string => {
    if (value === undefined || value === null || typeof value !== 'number' || !isFinite(value)) return '-';
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

  const healthyCount = health?.services.filter(s => s.status === 'healthy').length ?? 0;
  const totalCount = health?.services.length ?? 0;
  const servicesStatusText = `${healthyCount}/${totalCount} healthy`;
  const servicesStatusColor = healthyCount === totalCount ? 'healthy' : healthyCount === 0 ? 'unhealthy' : 'warning';

  return (
    <div className="app grafana-style">
      {/* Top Navigation Bar */}
      <header className="dashboard-navbar">
        <div className="navbar-left">
          <div className="dashboard-logo">
            <span className="logo-icon">◈</span>
            <span className="logo-text">Midnight L2</span>
          </div>
          <div className="navbar-divider" />
          <h1 className="dashboard-title">Service Dashboard</h1>
        </div>
        <div className="navbar-center">
          {health && (
            <div className={`system-status-pill ${getStatusColor(health.status)}`}>
              <span className="status-dot-small" />
              <span>System {health.status}</span>
            </div>
          )}
        </div>
        <div className="navbar-right">
          <div className="refresh-controls">
            <select 
              className="refresh-interval-select"
              value={refreshInterval}
              onChange={(e) => setRefreshInterval(Number(e.target.value))}
            >
              <option value={5000}>5s</option>
              <option value={10000}>10s</option>
              <option value={30000}>30s</option>
              <option value={60000}>1m</option>
            </select>
            <label className="auto-refresh-toggle">
              <input
                type="checkbox"
                checked={autoRefresh}
                onChange={(e) => setAutoRefresh(e.target.checked)}
              />
              <span className="toggle-slider" />
            </label>
            <button className="icon-btn refresh" onClick={() => { loadHealth(); loadMetrics(); }} disabled={loading}>
              <span className={loading ? 'spinning' : ''}>↻</span>
            </button>
          </div>
          {health && (
            <span className="last-updated">
              {new Date(health.checkedAt).toLocaleTimeString()}
            </span>
          )}
        </div>
      </header>

      <main className="dashboard-main">
        {error ? (
          <div className="error-panel">
            <div className="error-icon">⚠</div>
            <div className="error-content">
              <strong>Connection Error</strong>
              <p>Service controller unreachable at <code>http://127.0.0.1:9090</code></p>
            </div>
          </div>
        ) : loading && !health ? (
          <div className="loading-panel">
            <div className="loading-spinner" />
            <span>Loading dashboard...</span>
          </div>
        ) : health ? (
          <>
            {/* Row: Service Controls */}
            <DashboardRow title="Service Controls" defaultExpanded={true}>
              <div className="controls-panel">
                <div className="controls-grid">
                  <button
                    className="control-btn start"
                    onClick={() => handleAction('start')}
                    disabled={actionLoading !== null}
                  >
                    <span className="btn-icon">▶</span>
                    {actionLoading === 'start' ? 'Starting...' : 'Start'}
                  </button>
                  <button
                    className="control-btn stop"
                    onClick={() => handleAction('stop')}
                    disabled={actionLoading !== null}
                  >
                    <span className="btn-icon">■</span>
                    {actionLoading === 'stop' ? 'Stopping...' : 'Stop'}
                  </button>
                  <button
                    className="control-btn restart"
                    onClick={() => handleAction('restart')}
                    disabled={actionLoading !== null}
                  >
                    <span className="btn-icon">↻</span>
                    {actionLoading === 'restart' ? 'Restarting...' : 'Restart'}
                  </button>
                  <button
                    className="control-btn clean"
                    onClick={() => handleAction('clean')}
                    disabled={actionLoading !== null}
                  >
                    <span className="btn-icon">🗑</span>
                    {actionLoading === 'clean' ? 'Cleaning...' : 'Clean Data'}
                  </button>
                </div>
                {actionResult && (
                  <div className={`action-toast ${actionResult.success ? 'success' : 'error'}`}>
                    {actionResult.success ? '✓' : '✗'} {actionResult.message}
                  </div>
                )}
              </div>
            </DashboardRow>

            {/* Row: Quick Stats Overview */}
            <DashboardRow title="Overview" defaultExpanded={true}>
              <div className="overview-panels">
                {/* Key Metrics Stats */}
                <div className="stat-panel highlight">
                  <div className="stat-panel-label">TPS</div>
                  <div className="stat-panel-value">{formatTps(metrics?.tps?.tps)}</div>
                  <div className="stat-panel-subtext">transactions/sec</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Total Transactions</div>
                  <div className="stat-panel-value">{formatNumber(metrics?.totalTransactions?.total_transactions, 0)}</div>
                  <div className="stat-panel-subtext">cumulative</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Failed Rate</div>
                  <div className={`stat-panel-value ${(metrics?.failedTransactionsRate?.rate_percent ?? 0) > 5 ? 'warning' : ''}`}>
                    {formatPercent(metrics?.failedTransactionsRate?.rate_percent)}
                  </div>
                  <div className="stat-panel-subtext">failure rate</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Value Spent (24h)</div>
                  <div className="stat-panel-value">{formatTokenAmount(metrics?.tokenValueSpent?.value_spent)}</div>
                  <div className="stat-panel-subtext">tokens</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Avg Tx Size</div>
                  <div className="stat-panel-value">{formatTokenAmount(metrics?.averageTransactionSize?.average_amount)}</div>
                  <div className="stat-panel-subtext">tokens (24h)</div>
                </div>
                <div className="stat-panel">
                  <div className="stat-panel-label">Token Velocity</div>
                  <div className="stat-panel-value">{formatNumber(metrics?.tokenVelocity?.token_velocity, 4)}</div>
                  <div className="stat-panel-subtext">turnover rate</div>
                </div>
              </div>
            </DashboardRow>

            {/* Row: System Resources */}
            <DashboardRow title="System Resources" defaultExpanded={true}>
              <SystemStatsPanel autoRefresh={autoRefresh} />
            </DashboardRow>

            {/* Row: Services */}
            <DashboardRow 
              title="Services" 
              defaultExpanded={true}
              badge={servicesStatusText}
              badgeColor={servicesStatusColor}
            >
              <div className="services-panel">
                <div className="services-table">
                  <div className="table-header">
                    <div className="col-status">Status</div>
                    <div className="col-name">Service</div>
                    <div className="col-endpoint">Endpoint</div>
                    <div className="col-latency">Latency</div>
                    <div className="col-error">Error</div>
                  </div>
                  {health.services.map((service) => (
                    <div key={service.name} className={`table-row ${getStatusColor(service.status)}`}>
                      <div className="col-status">
                        <span className="status-indicator-dot" />
                      </div>
                      <div className="col-name">{service.name}</div>
                      <div className="col-endpoint">
                        <code>{service.url}</code>
                      </div>
                      <div className="col-latency">{formatResponseTime(service.response_time_ms)}</div>
                      <div className="col-error">{service.error || '-'}</div>
                    </div>
                  ))}
                </div>
              </div>
            </DashboardRow>

            {/* Row: Network Metrics Details */}
            <DashboardRow title="Network Metrics" defaultExpanded={false}>
              <div className="metrics-detail-panel">
                {metrics?.error ? (
                  <div className="panel-error">Metrics unavailable: {metrics.error}</div>
                ) : (
                  <div className="metrics-detail-grid">
                    {/* TPS Details */}
                    <div className="metric-detail-card">
                      <div className="metric-card-header">
                        <span className="metric-icon">⚡</span>
                        <h3>Transactions Per Second</h3>
                      </div>
                      <div className="metric-big-value">{formatTps(metrics?.tps?.tps)}</div>
                      <div className="metric-card-stats">
                        <div className="stat-row">
                          <span className="stat-label">Delta Transactions</span>
                          <span className="stat-value">{formatNumber(metrics?.tps?.delta_transactions, 0)}</span>
                        </div>
                      </div>
                    </div>

                    {/* Transaction Size */}
                    <div className="metric-detail-card">
                      <div className="metric-card-header">
                        <span className="metric-icon">📏</span>
                        <h3>Transaction Sizes</h3>
                      </div>
                      <div className="metric-card-stats">
                        <div className="stat-row">
                          <span className="stat-label">Average (24h)</span>
                          <span className="stat-value">{formatTokenAmount(metrics?.averageTransactionSize?.average_amount)}</span>
                        </div>
                        <div className="stat-row">
                          <span className="stat-label">Median (24h)</span>
                          <span className="stat-value">{formatTokenAmount(metrics?.medianTransactionSize?.median_amount)}</span>
                        </div>
                      </div>
                    </div>

                    {/* Token Economics */}
                    <div className="metric-detail-card">
                      <div className="metric-card-header">
                        <span className="metric-icon">💰</span>
                        <h3>Token Economics</h3>
                      </div>
                      <div className="metric-card-stats">
                        <div className="stat-row">
                          <span className="stat-label">Value Spent (24h)</span>
                          <span className="stat-value">{formatTokenAmount(metrics?.tokenValueSpent?.value_spent)}</span>
                        </div>
                        <div className="stat-row">
                          <span className="stat-label">Token Velocity</span>
                          <span className="stat-value">{formatNumber(metrics?.tokenVelocity?.token_velocity, 4)}</span>
                        </div>
                        <div className="stat-row">
                          <span className="stat-label">Total Supply</span>
                          <span className="stat-value">{formatTokenAmount(metrics?.tokenVelocity?.total_tokens)}</span>
                        </div>
                      </div>
                    </div>

                    {/* Failed Transactions */}
                    <div className="metric-detail-card">
                      <div className="metric-card-header">
                        <span className="metric-icon">⚠️</span>
                        <h3>Transaction Health</h3>
                      </div>
                      <div className="metric-card-stats">
                        <div className="stat-row">
                          <span className="stat-label">Failure Rate</span>
                          <span className={`stat-value ${(metrics?.failedTransactionsRate?.rate_percent ?? 0) > 5 ? 'warning' : ''}`}>
                            {formatPercent(metrics?.failedTransactionsRate?.rate_percent)}
                          </span>
                        </div>
                        <div className="stat-row">
                          <span className="stat-label">Failed Txs</span>
                          <span className="stat-value">{formatNumber(metrics?.failedTransactionsRate?.failed_transactions, 0)}</span>
                        </div>
                        <div className="stat-row">
                          <span className="stat-label">Total Txs</span>
                          <span className="stat-value">{formatNumber(metrics?.failedTransactionsRate?.total_transactions, 0)}</span>
                        </div>
                      </div>
                    </div>
                  </div>
                )}
              </div>
            </DashboardRow>

            {/* Row: Historic Charts */}
            <DashboardRow title="Historic Data" defaultExpanded={true}>
              <MetricsCharts autoRefresh={autoRefresh} />
            </DashboardRow>

            {/* Row: Service Logs */}
            <DashboardRow title="Service Logs" defaultExpanded={false}>
              <Terminal />
            </DashboardRow>
          </>
        ) : null}
      </main>

      <footer className="dashboard-footer">
        <span>Midnight L2 Service Controller</span>
        <code>http://127.0.0.1:9090</code>
      </footer>
    </div>
  );
}

export default App;
