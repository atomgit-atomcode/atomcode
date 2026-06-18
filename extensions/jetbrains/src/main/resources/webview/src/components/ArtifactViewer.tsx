import React from 'react';
import type { ArtifactViewModel } from '../state/types';

interface Props { artifact: ArtifactViewModel }

export const ArtifactViewer: React.FC<Props> = ({ artifact }) => (
  <div className="artifact-viewer">
    <div className="artifact-header">
      <span>{artifact.title ?? artifact.artifact_type} {artifact.language ? `(${artifact.language})` : ''}</span>
      <span>{artifact.status}</span>
    </div>
    <pre className="artifact-content"><code>{artifact.content}</code></pre>
  </div>
);
