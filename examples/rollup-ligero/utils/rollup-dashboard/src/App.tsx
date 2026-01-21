import { useState, useEffect, useCallback } from 'react';
import type { HealthResponse, ActionType, ActionResult } from './types';
import { fetchHealth, performAction } from './api';
import './styles.css';

function App() {
  const [health, setHealth] = useState<HealthResponse | null>(null);
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

  useEffect(() => {
    loadHealth();
    if (autoRefresh) {
      const interval = setInterval(loadHealth, 5000);
      return () => clearInterval(interval);
    }
  }, [loadHealth, autoRefresh]);

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
