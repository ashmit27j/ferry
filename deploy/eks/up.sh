#!/usr/bin/env bash
# Stands up Ferry on a new EKS cluster: eksctl cluster, AWS Load Balancer
# Controller, kube-prometheus-stack (Prometheus, Grafana with the Ferry
# dashboard), the Ferry image in ECR, and the Ferry chart with a Gateway.
#
# Needs: aws (credentials configured), eksctl, helm, kubectl, docker, and
# the Linux release binary in the `ferry-target` volume (bench/README.md).
#
#   CLUSTER=ferry-staging REGION=us-east-1 deploy/eks/up.sh
#   STEPS="ferry" deploy/eks/up.sh        # rerun only some steps
#
# Steps: cluster lbc monitoring image ferry. Tear down with
#   eksctl delete cluster --name $CLUSTER --region $REGION
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
EKS="$ROOT/deploy/eks"
export CLUSTER=${CLUSTER:-ferry-staging}
export REGION=${REGION:-us-east-1}
export AWS_REGION=$REGION
STEPS=${STEPS:-cluster lbc monitoring image ferry}
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
REPO="$ACCOUNT.dkr.ecr.$REGION.amazonaws.com/ferry"
TAG=${TAG:-$(git -C "$ROOT" rev-parse --short HEAD)}
step() { [[ " $STEPS " == *" $1 "* ]]; }

if step cluster; then
  echo "== cluster $CLUSTER in $REGION (account $ACCOUNT)"
  envsubst < "$EKS/cluster.yaml" | eksctl create cluster -f -
fi
aws eks update-kubeconfig --name "$CLUSTER" --region "$REGION" >/dev/null

if step lbc; then
  echo "== AWS Load Balancer Controller"
  helm repo add eks https://aws.github.io/eks-charts >/dev/null
  helm repo update eks >/dev/null
  VPC=$(aws eks describe-cluster --name "$CLUSTER" --region "$REGION" \
    --query cluster.resourcesVpcConfig.vpcId --output text)
  helm upgrade --install aws-load-balancer-controller eks/aws-load-balancer-controller \
    -n kube-system --set clusterName="$CLUSTER" --set region="$REGION" --set vpcId="$VPC" \
    --set serviceAccount.create=false --set serviceAccount.name=aws-load-balancer-controller \
    --wait
fi

if step monitoring; then
  echo "== kube-prometheus-stack"
  helm repo add prometheus-community https://prometheus-community.github.io/helm-charts >/dev/null
  helm repo update prometheus-community >/dev/null
  kubectl create namespace monitoring --dry-run=client -o yaml | kubectl apply -f -
  kubectl -n monitoring create configmap ferry-dashboard \
    --from-file=ferry.json="$ROOT/deploy/grafana/ferry-dashboard.json" \
    --dry-run=client -o yaml | kubectl label --local -f - grafana_dashboard=1 -o yaml \
    | kubectl apply -f -
  helm upgrade --install kube-prometheus-stack prometheus-community/kube-prometheus-stack \
    -n monitoring \
    --set prometheus.prometheusSpec.podMonitorSelectorNilUsesHelmValues=false \
    --set prometheus.prometheusSpec.ruleSelectorNilUsesHelmValues=false \
    --wait --timeout 10m
fi

if step image; then
  echo "== image $REPO:$TAG"
  aws ecr describe-repositories --region "$REGION" --repository-names ferry >/dev/null 2>&1 \
    || aws ecr create-repository --region "$REGION" --repository-name ferry \
         --image-scanning-configuration scanOnPush=true >/dev/null
  CTX=$(mktemp -d)
  docker run --rm -v ferry-target:/target:ro -v "$CTX:/out" rust:1.99.0-bookworm \
    objcopy --strip-debug /target/release/ferry /out/ferry
  docker build --platform linux/amd64 -t "$REPO:$TAG" -f "$EKS/Dockerfile.prebuilt" "$CTX"
  aws ecr get-login-password --region "$REGION" \
    | docker login --username AWS --password-stdin "${REPO%/ferry}"
  docker push "$REPO:$TAG"
  rm -rf "$CTX"
fi

if step ferry; then
  echo "== ferry"
  helm upgrade --install ferry "$ROOT/deploy/helm/ferry" -n ferry-system --create-namespace \
    -f "$EKS/values-eks.yaml" --set image.repository="$REPO" --set image.tag="$TAG" \
    --wait --timeout 10m
  echo "== waiting for the NLB address"
  for _ in $(seq 1 60); do
    ADDR=$(kubectl get gateway -n ferry-system ferry -o jsonpath='{.status.addresses[0].value}' 2>/dev/null || true)
    [ -n "$ADDR" ] && break
    sleep 10
  done
  echo "Gateway address: ${ADDR:-not yet assigned (kubectl get gateway -n ferry-system ferry)}"
fi

echo
echo "Grafana:  kubectl -n monitoring port-forward svc/kube-prometheus-stack-grafana 3000:80"
echo "          http://localhost:3000  (admin / kubectl -n monitoring get secret kube-prometheus-stack-grafana -o jsonpath='{.data.admin-password}' | base64 -d)"
echo "Routes:   attach HTTPRoutes to Gateway ferry-system/ferry (deploy/eks/route.example.yaml)"
